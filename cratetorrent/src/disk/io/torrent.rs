use std::{
    collections::{BTreeMap, HashMap},
    fs,
    sync::{
        self,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
};

use chashmap::CHashMap;
use tokio::task;

use crate::{
    disk::{
        error::*,
        io::{
            file::TorrentFile,
            piece::{self, Piece},
        },
    },
    peer,
    storage_info::{FsStructure, StorageInfo},
    torrent::{self, PieceCompletion},
    Block, BlockInfo, CachedBlock, PieceIndex,
};

/// Torrent information related to disk IO.
///
/// Contains the in-progress pieces (i.e. the write buffer), metadata about
/// torrent's download and piece sizes, etc.
pub(crate) struct Torrent {
    /// All information concerning this torrent's storage.
    info: StorageInfo,

    /// The in-progress piece downloads and disk writes. This is the torrent's
    /// disk write buffer. Each piece is mapped to its index for faster lookups.
    // TODO(https://github.com/mandreyel/cratetorrent/issues/22): Currently
    // there is no upper bound on this.
    write_buf: HashMap<PieceIndex, Piece>,

    /// Contains the fields that may be accessed by other threads.
    ///
    /// This is an optimization to avoid having to call
    /// `Arc::clone(&self.field)` for each of the contained fields when sending
    /// them to an IO worker threads. See more in [`ThreadContext`].
    thread_ctx: Arc<ThreadContext>,

    /// The concatenation of all expected piece hashes.
    piece_hashes: Vec<u8>,
}

/// Contains fields that are commonly accessed by torrent's IO threads.
///
/// We're using blocking IO to read things from disk and so such operations need to be
/// spawned on a separate thread to not block the tokio reactor driving the
/// disk task.
/// But these threads need some fields from torrent and so those fields
/// would need to be in an arc each. With this optimization, only this
/// struct needs to be in an arc and thus only a single atomic increment has to
/// be made when sending the contained fields across threads.
struct ThreadContext {
    /// The channel used to alert a torrent that a block has been written to
    /// disk and/or a piece was completed.
    chan: torrent::Sender,

    /// The read cache that caches entire pieces.
    ///
    /// The piece is stored as a list of 16 KiB blocks since that is what peers
    /// are going to request, so this avoids extra copies. Blocks are ordered.
    ///
    /// Every time a block read is issued it is checked if it's already cached
    /// here. If not, the whole pieces is read from disk and placed in the cache.
    ///
    /// The cache is in a read-write lock as an optimization for saturated
    /// caches: once we have many pieces in cache it is expected that most reads
    /// will hit the cache rather than the disk. In such cases it's not
    /// necessary to write lock the cache as it would on a cache misses, and
    /// this avoids concurrent reads in later stages.
    // TODO(https://github.com/mandreyel/cratetorrent/issues/22): Currently
    // there is no upper bound on this. Consider using an LRU cache or similar.
    read_cache: CHashMap<PieceIndex, Vec<CachedBlock>>,

    /// Handles of all files in torrent, opened in advance during torrent
    /// creation.
    ///
    /// Each writer thread will get exclusive access to the file handle it
    /// needs, referring to it directly in the vector (hence the arc).
    /// Multiple readers may read from the same file, but not while there is
    /// a pending write.
    ///
    /// Later we will need to make file access more granular, as multiple
    /// concurrent writes to the same file that don't overlap are safe to do.
    // TODO: consider improving concurreny by allowing concurrent reads and
    // writes on different parts of the file using byte-range locking
    // TODO: Is there a way to avoid copying `FileInfo`s here from
    // `self.info.structure`? We could just pass the file info on demand, but
    // that woudl require reallocating this vector every time (to pass a new
    // vector of pairs of `TorrentFile` and `FileInfo`).
    files: Vec<sync::RwLock<TorrentFile>>,

    /// Various disk IO related statistics.
    ///
    /// Stas are atomically updated by the IO worker threads themselves.
    stats: Stats,
}

#[derive(Default)]
struct Stats {
    /// The number of bytes successfully written to disk.
    write_count: AtomicU64,
    /// The number of times we failed to write to disk.
    write_failure_count: AtomicUsize,
    /// The number of bytes successfully read from disk.
    read_count: AtomicU64,
    /// The number of times we failed to read from disk.
    read_failure_count: AtomicUsize,
}

impl Torrent {
    /// handles.
    /// Creates the file system structure of the torrent and opens the file
    ///
    /// For a single file, there is a path validity check and then the file is
    /// opened. For multi-file torrents, if there are any subdirectories in the
    /// torrent archive, they are created and all files are opened.
    pub fn new(
        info: StorageInfo,
        piece_hashes: Vec<u8>,
        torrent_chan: torrent::Sender,
    ) -> Result<Self, NewTorrentError> {
        // TODO: since this is done as part of a tokio::task, should we use
        // tokio_fs here?
        if !info.download_dir.is_dir() {
            log::warn!(
                "Creating missing download directory {:?}",
                info.download_dir
            );
            fs::create_dir_all(&info.download_dir)?;
            log::info!("Download directory {:?} created", info.download_dir);
        }

        let files = match &info.structure {
            FsStructure::File(file) => {
                log::debug!(
                    "Torrent is single {} bytes long file {:?}",
                    file.len,
                    file.path
                );
                vec![sync::RwLock::new(TorrentFile::new(
                    &info.download_dir,
                    file.clone(),
                )?)]
            }
            FsStructure::Archive { files } => {
                debug_assert!(!files.is_empty());
                log::debug!("Torrent is multi file: {:?}", files);
                log::debug!("Setting up directory structure");

                let mut torrent_files = Vec::with_capacity(files.len());
                for file in files.iter() {
                    let path = info.download_dir.join(&file.path);
                    // file or subdirectory in download root must not exist if
                    // download root does not exists
                    debug_assert!(!path.exists());
                    debug_assert!(path.is_absolute());

                    // get the parent of the file path: if there is one (i.e.
                    // this is not a file in the torrent root), and doesn't
                    // exist, create it
                    if let Some(subdir) = path.parent() {
                        if !subdir.exists() {
                            log::info!("Creating torrent subdir {:?}", subdir);
                            fs::create_dir_all(&subdir).map_err(|e| {
                                log::error!(
                                    "Failed to create subdir {:?}",
                                    subdir
                                );
                                NewTorrentError::Io(e)
                            })?;
                        }
                    }

                    // open the file and get a handle to it
                    //
                    // TODO: is there a clean way of avoiding creating the path
                    // buffer twice?
                    torrent_files.push(sync::RwLock::new(TorrentFile::new(
                        &info.download_dir,
                        file.clone(),
                    )?));
                }
                torrent_files
            }
        };

        Ok(Self {
            info,
            write_buf: HashMap::new(),
            thread_ctx: Arc::new(ThreadContext {
                chan: torrent_chan,
                read_cache: CHashMap::new(),
                files,
                stats: Stats::default(),
            }),
            piece_hashes,
        })
    }

    pub async fn write_block(
        &mut self,
        info: BlockInfo,
        data: Vec<u8>,
    ) -> Result<()> {
        log::trace!("Saving block {} to disk", info);

        let piece_index = info.piece_index;
        if !self.write_buf.contains_key(&piece_index) {
            if let Err(e) = self.start_new_piece(info.piece_index) {
                self.thread_ctx
                    .chan
                    .send(torrent::Message::PieceCompletion(Err(e)))?;
                // return with ok as the disk task itself shouldn't be aborted
                // due to invalid input
                return Ok(());
            }
        }
        let piece = self
            .write_buf
            .get_mut(&piece_index)
            .expect("Newly inserted piece not present");

        piece.enqueue_block(info.offset, data);

        // if the piece has all its blocks, it means we can hash it and save it
        // to disk and clear its write buffer
        if piece.is_complete() {
            // TODO: remove from in memory store only if the disk write
            // succeeded (otherwise we need to retry later)
            let piece = self.write_buf.remove(&piece_index).unwrap();

            log::debug!(
                "Piece {} is complete ({} bytes), flushing {} block(s) to disk",
                info.piece_index,
                piece.len,
                piece.blocks.len()
            );

            // don't block the reactor with the potentially expensive hashing
            // and sync file writing
            let torrent_piece_offset =
                self.info.torrent_piece_offset(piece_index);
            let ctx = Arc::clone(&self.thread_ctx);
            task::spawn_blocking(move || {
                let is_piece_valid = piece.matches_hash();

                // save piece to disk if it's valid
                if is_piece_valid {
                    log::debug!(
                        "Piece {} is valid, writing to disk",
                        piece_index
                    );

                    if let Err(e) =
                        piece.write(torrent_piece_offset, &*ctx.files)
                    {
                        log::error!(
                            "Error writing piece {} to disk: {}",
                            piece_index,
                            e
                        );
                        // TODO(https://github.com/mandreyel/cratetorrent/issues/23):
                        // also place back piece write buffer in torrent and
                        // retry later
                        ctx.stats
                            .write_failure_count
                            .fetch_add(1, Ordering::Relaxed);
                        // alert torrent of block write failure
                        ctx.chan
                            .send(torrent::Message::PieceCompletion(Err(e)))
                            .map_err(|e| {
                                log::error!(
                                    "Error sending piece result: {}",
                                    e
                                );
                                e
                            })
                            .ok();
                        return;
                    }

                    log::debug!("Wrote piece {} to disk", piece_index);
                    ctx.stats
                        .write_count
                        .fetch_add(piece.len as u64, Ordering::Relaxed);
                } else {
                    log::warn!("Piece {} is not valid", info.piece_index);
                }

                // alert torrent of piece completion and hash result
                ctx.chan
                    .send(torrent::Message::PieceCompletion(Ok(
                        PieceCompletion {
                            index: piece_index,
                            is_valid: is_piece_valid,
                        },
                    )))
                    .map_err(|e| {
                        log::error!("Error sending piece result: {}", e);
                        e
                    })
                    .ok();
            });
        }

        Ok(())
    }

    /// Starts a new in-progress piece, creating metadata for it in self.
    ///
    /// This involves getting the expected hash of the piece, its length, and
    /// calculating the files that it intersects.
    fn start_new_piece(
        &mut self,
        piece_index: PieceIndex,
    ) -> Result<(), WriteError> {
        log::trace!("Creating piece {} write buffer", piece_index);

        // get the position of the piece in the concatenated hash string
        let hash_pos = piece_index * 20;
        if hash_pos + 20 > self.piece_hashes.len() {
            log::error!("Piece index {} is invalid", piece_index);
            return Err(WriteError::InvalidPieceIndex);
        }

        let hash_slice = &self.piece_hashes[hash_pos..hash_pos + 20];
        let mut expected_hash = [0; 20];
        expected_hash.copy_from_slice(hash_slice);
        log::debug!(
            "Piece {} expected hash {}",
            piece_index,
            hex::encode(&expected_hash)
        );

        // TODO: consider using expect here as piece index should be verified in
        // Torrent::write_block
        let len = self
            .info
            .piece_len(piece_index)
            .map_err(|_| WriteError::InvalidPieceIndex)?;
        log::debug!("Piece {} is {} bytes long", piece_index, len);

        let file_range = self
            .info
            .files_intersecting_piece(piece_index)
            .map_err(|_| WriteError::InvalidPieceIndex)?;
        log::debug!("Piece {} intersects files: {:?}", piece_index, file_range);

        let piece = Piece {
            expected_hash,
            len,
            blocks: BTreeMap::new(),
            file_range,
        };
        self.write_buf.insert(piece_index, piece);

        Ok(())
    }

    /// Returns the specified block via the sender, either from the read cache
    /// or from the disk.
    ///
    /// If the block info refers to an invalid piece, an error is returned.
    /// If the block info is correct but the underlying file does not yet
    /// contain the data, an error is returned.
    ///
    /// On a cache miss, the method reads in the whole piece of the block,
    /// stores the piece in memory, and returns the requested block via the
    /// sender. The rationale is that if a peer is requesting a block in piece,
    /// it will very likely request further blocks in the same piece, so we want
    /// to prepare for it. This is referred to as a "read cache line", much like
    /// how the CPU pulls in the next 64 bytes of the program into its L1 cache
    /// when hitting a cache miss.
    /// For now, this is simplified in that we don't pull in blocks from the
    /// next piece. Later, we will make the read cache line size configurable
    /// and it will be applied across piece boundaries.
    pub async fn read_block(
        &self,
        block_info: BlockInfo,
        result_chan: peer::Sender,
    ) -> Result<()> {
        log::trace!("Reading {} from disk", block_info);

        let piece_index = block_info.piece_index;
        let block_index = block_info.index_in_piece();

        // check if piece is in the read cache
        if let Some(blocks) = self.thread_ctx.read_cache.get(&piece_index) {
            log::debug!("Piece {} is in the read cache", piece_index);
            // the block's index in piece may be invalid
            if block_index >= blocks.len() {
                log::debug!(
                    "Piece {} block offset {} is invalid",
                    piece_index,
                    block_info.offset
                );
                self.thread_ctx.chan.send(torrent::Message::ReadError {
                    block_info,
                    error: ReadError::InvalidBlockOffset,
                })?;
                // the disk task itself mustn't be aborted due to invalid input
                return Ok(());
            }

            // return block via sender
            let block = Arc::clone(&blocks[block_index]);
            result_chan
                .send(peer::Command::Block(Block::new(block_info, block)))?;
        } else {
            // otherwise read in the piece from disk
            log::debug!(
                "Piece {} not in the read cache, reading from disk",
                piece_index
            );

            let file_range = match self
                .info
                .files_intersecting_piece(piece_index)
            {
                Ok(file_range) => file_range,
                Err(_) => {
                    log::error!("Piece {} not in file", piece_index);
                    self.thread_ctx.chan.send(torrent::Message::ReadError {
                        block_info,
                        error: ReadError::InvalidPieceIndex,
                    })?;
                    // return with ok as the disk task itself shouldn't be aborted
                    // due to invalid input
                    return Ok(());
                }
            };

            // Checking if the file pointed to by info has been downloaded yet
            // is done implicitly as part of the read operation below: if we
            // can't read any bytes, the file likely does not exist.

            // don't block the reactor with blocking disk IO
            let torrent_piece_offset =
                self.info.torrent_piece_offset(piece_index);
            let piece_len = self.info.piece_len(piece_index)?;
            let ctx = Arc::clone(&self.thread_ctx);
            task::spawn_blocking(move || {
                match piece::read(
                    torrent_piece_offset,
                    file_range,
                    &ctx.files[..],
                    piece_len,
                ) {
                    Ok(blocks) => {
                        log::debug!("Read piece {}", piece_index);
                        // pick requested block
                        let block = Arc::clone(&blocks[block_index]);

                        // Place piece in read cache. Another concurrent read
                        // could already have read the piece just before this
                        // thread, but replacing it shouldn't be an issue since
                        // we're reading the same data.
                        ctx.read_cache.insert(piece_index, blocks);
                        ctx.stats
                            .read_count
                            .fetch_add(piece_len as u64, Ordering::Relaxed);

                        // send block to peer
                        result_chan
                            .send(peer::Command::Block(Block::new(
                                block_info, block,
                            )))
                            .map_err(|e| {
                                log::error!(
                                    "Error sending block to peer: {}",
                                    e
                                );
                                e
                            })
                            .ok();
                    }
                    Err(e) => {
                        log::error!(
                            "Error reading piece {} from disk: {}",
                            piece_index,
                            e
                        );

                        ctx.stats
                            .read_failure_count
                            .fetch_add(1, Ordering::Relaxed);
                        ctx.chan
                            .send(torrent::Message::ReadError {
                                block_info,
                                error: e,
                            })
                            .map_err(|e| {
                                log::error!("Error sending read error: {}", e);
                                e
                            })
                            .ok();
                    }
                }
            });
        }

        Ok(())
    }
}
