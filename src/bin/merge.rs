use log::{debug, error, info, warn};
use qbit_rs::model::{Credential, Preferences, PieceState, TorrentContent, TorrentProperty};
use qbit_rs::Qbit;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};
use std::convert::TryInto;
use std::fs::OpenOptions;
use std::io::{prelude::*, BufReader, BufWriter};
use std::{fs::File};

#[derive(Debug, Serialize, Deserialize)]
struct Config {
    host: String,
    username: String,
    password: String,
}

impl Config {
    fn load(path: Option<&str>) -> Result<Self, Box<dyn std::error::Error>> {
        let path = path.unwrap_or("config.json");
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let config = serde_json::from_reader(reader)?;
        Ok(config)
    }
}

struct Torrent {
    hash: String,
    properties: TorrentProperty,
    content: Vec<TorrentContent>,
    pieces_states: Vec<PieceState>,
    pieces_hashes: Vec<[u8; 20]>,
}

impl Torrent {
    async fn new(api: &Qbit, hash: &str) -> Result<Self, qbit_rs::Error> {
        let pieces_hashes: Vec<[u8; 20]> = api
            .get_torrent_pieces_hashes(hash)
            .await?
            .iter()
            .map(|s| hex::decode(s).unwrap().try_into().unwrap())
            .collect();
        let pieces_states = api.get_torrent_pieces_states(hash).await?;
        let properties = api.get_torrent_properties(hash).await?;
        let content = api.get_torrent_contents(hash, None).await?;

        let torrent = Torrent {
            hash: hash.to_owned(),
            properties,
            content,
            pieces_states,
            pieces_hashes,
        };
        Ok(torrent)
    }

    fn piece_is_downloaded(&self, piece: &TorrentPiece) -> bool {
        let piece = match self.pieces_states.get(piece.idx) {
            Some(p) => p,
            None => return false,
        };
        match piece {
            PieceState::Downloaded => true,
            _ => false,
        }
    }
}

fn get_sha1(data: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(data);
    let sha1: [u8; 20] = hasher.finalize().into();
    sha1
}

#[derive(Debug, Copy, Clone)]
struct FileBlock {
    offset: u64,
    size: u64,
}

impl FileBlock {
    fn contains(&self, other: &Self) -> bool {
        self.offset <= other.offset && self.offset + self.size >= other.offset + other.size
    }
}

#[derive(Debug, Copy, Clone)]
enum Piece {
    VirtualPiece(VirtualPiece),
    TorrentPiece(TorrentPiece),
}

#[derive(Debug, Copy, Clone)]
struct VirtualPiece {
    offset: usize,
    piece_size: u64,
}

#[derive(Debug, Copy, Clone)]
struct TorrentPiece {
    idx: usize,
    piece_size: u64,
}

impl TorrentPiece {
    fn merge(list: &[TorrentPiece]) -> Option<VirtualPiece> {
        let first_piece = list.get(0)?;
        Some(VirtualPiece {
            offset: first_piece.idx * first_piece.piece_size as usize,
            piece_size: list.len() as u64 * first_piece.piece_size,
        })
    }
}

fn piece_to_file_block(
    torrent: &Torrent,
    piece: &Piece,
) -> Result<(String, FileBlock), Box<dyn std::error::Error>> {
    match *piece {
        Piece::TorrentPiece(piece) => {
            let mut offset = piece.idx as u64 * piece.piece_size;
            for f in &torrent.content {
                if offset < f.size {
                    let file_block = FileBlock {
                        offset,
                        size: piece.piece_size,
                    };
                    return Ok((f.name.clone(), file_block));
                } else {
                    offset -= f.size;
                }
            }
            Err("Piece outside of torrent".into())
        }
        Piece::VirtualPiece(piece) => {
            let mut offset = piece.offset as u64;
            for f in &torrent.content {
                if offset < f.size {
                    let file_block = FileBlock {
                        offset,
                        size: piece.piece_size,
                    };
                    return Ok((f.name.clone(), file_block));
                } else {
                    offset -= f.size;
                }
            }
            Err("Piece outside of torrent".into())
        }
    }
}

fn file_block_to_pieces(
    torrent: &Torrent,
    path: &str,
    file_block: &FileBlock,
) -> Result<Vec<TorrentPiece>, Box<dyn std::error::Error>> {
    let piece_size = torrent.properties.piece_size.unwrap() as u64;
    let mut offset = 0;
    for f in &torrent.content {
        if f.name == path {
            if file_block.offset > f.size {
                return Err(format!("Offset beyond file {} {}", file_block.offset, path).into());
            } else {
                offset += file_block.offset;
                let start_idx = (offset / piece_size) as usize;
                let end_idx = ((offset + file_block.size).div_ceil(piece_size)) as usize;

                let result: Vec<TorrentPiece> = (start_idx..end_idx)
                    .map(|idx| TorrentPiece { idx, piece_size })
                    .collect();

                return Ok(result);
            }
        } else {
            offset += f.size;
        }
    }
    Err(format!("File not found {:?}", path).into())
}

fn convert_filename(
    same_files: &[(Vec<String>, Vec<String>)],
    filename: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    for (list_a, list_b) in same_files {
        for name in list_a {
            if name == filename {
                return Ok(list_b[0].clone());
            }
        }
        for name in list_b {
            if name == filename {
                return Ok(list_a[0].clone());
            }
        }
    }
    Err(format!("File not found {:?}", filename).into())
}

fn get_read_file(
    preferences: &Preferences,
    torrent_property: &TorrentProperty,
    path: &str,
) -> std::io::Result<BufReader<File>> {
    let path = if torrent_property.pieces_num.unwrap() == torrent_property.pieces_have.unwrap() {
        format!("{}/{}", torrent_property.save_path.as_ref().unwrap(), path)
    } else {
        format!("{}/{}", preferences.temp_path.as_ref().unwrap(), path)
    };

    let f = OpenOptions::new().read(true).open(path)?;
    Ok(BufReader::new(f))
}

fn get_write_file(
    preferences: &Preferences,
    torrent_property: &TorrentProperty,
    path: &str,
) -> std::io::Result<BufWriter<File>> {
    let path = if torrent_property.pieces_num.unwrap() == torrent_property.pieces_have.unwrap() {
        format!("{}/{}", torrent_property.save_path.as_ref().unwrap(), path)
    } else {
        format!("{}/{}", preferences.temp_path.as_ref().unwrap(), path)
    };

    let f = OpenOptions::new().write(true).open(path)?;
    Ok(BufWriter::new(f))
}

fn write_piece(f: &mut BufWriter<File>, file_block: FileBlock, data: &[u8]) -> std::io::Result<()> {
    f.seek(std::io::SeekFrom::Start(file_block.offset))?;
    f.write_all(data)?;
    f.flush()
}

fn read_piece(f: &mut BufReader<File>, file_block: FileBlock) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0; file_block.size as usize];
    f.seek(std::io::SeekFrom::Start(file_block.offset))?;
    f.read_exact(&mut buf)?;
    Ok(buf)
}

fn find_same_size_files(t1: &Torrent, t2: &Torrent) -> Vec<(Vec<String>, Vec<String>)> {
    let mut t1_files: HashMap<u64, Vec<String>> = HashMap::new();
    for f in t1.content.iter() {
        let size = f.size;
        let name = f.name.clone();
        t1_files.entry(size).or_default().push(name);
    }
    let mut t2_files: HashMap<u64, Vec<String>> = HashMap::new();
    for f in t2.content.iter() {
        let size = f.size;
        let name = f.name.clone();
        t2_files.entry(size).or_default().push(name);
    }

    let t1_keys: HashSet<u64> = t1_files.keys().copied().collect();
    let t2_keys: HashSet<u64> = t2_files.keys().copied().collect();

    let mut common_files: Vec<(Vec<String>, Vec<String>)> = Vec::new();
    for common in t1_keys.intersection(&t2_keys) {
        let a = t1_files.get(common).unwrap().clone();
        let b = t2_files.get(common).unwrap().clone();
        common_files.push((a, b));
    }
    common_files
}

fn get_missing_pieces(torrent: &Torrent, path: &str) -> Vec<usize> {
    let piece_size = torrent.properties.piece_size.unwrap() as u64;
    let offset = get_file_offset(&torrent.content, path).unwrap();
    let starting_idx = offset / piece_size;
    let file_size = torrent
        .content
        .iter()
        .find(|f| f.name == path)
        .expect("File not found")
        .size;
    let n_pieces = file_size / piece_size;
    let last_idx = starting_idx + n_pieces;

    let missing_pieces_idx: Vec<usize> = torrent
        .pieces_states
        .iter()
        .enumerate()
        .filter_map(|(idx, piece_state)| {
            if idx as u64 >= starting_idx
                && idx as u64 <= last_idx
                && piece_state != &PieceState::Downloaded
            {
                Some(idx)
            } else {
                None
            }
        })
        .collect();

    missing_pieces_idx
}

fn get_file_offset(
    torrent_content: &[TorrentContent],
    path: &str,
) -> Result<u64, Box<dyn std::error::Error>> {
    let mut offset = 0;
    let mut found = false;
    for file in torrent_content.iter() {
        if file.name == path {
            found = true;
            break;
        }
        offset += file.size;
    }

    if !found {
        return Err(format!("File not found: {:?}", path).into());
    }
    Ok(offset)
}

async fn merge_torrents(
    api: &Qbit,
    src_hash: &str,
    dst_hash: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let preferences = api.get_preferences().await.unwrap();

    info!("src_hash: {}", src_hash);
    info!("dst_hash: {}", dst_hash);

    let src_torrent: Torrent = Torrent::new(&api, src_hash).await?;
    let dst_torrent = Torrent::new(&api, dst_hash).await?;
    api.pause_torrents(&[dst_torrent.hash.clone()]).await?;

    let mut unavailable_pieces = 0;
    let mut data_outside_file_block = 0;
    let mut restored_pieces = 0;

    debug!(
        "src_torrent.piece_size={}",
        src_torrent.properties.piece_size.unwrap()
    );
    info!("src content:");
    for f in &src_torrent.content {
        info!("{:10} {}", f.size, &f.name);
    }
    debug!(
        "dst_torrent.piece_size={}",
        dst_torrent.properties.piece_size.unwrap()
    );
    info!("dst content:");
    for f in &dst_torrent.content {
        info!("{:10} {}", f.size, &f.name);
    }

    let same_files = find_same_size_files(&src_torrent, &dst_torrent);
    info!("same files: {:?}", &same_files);

    for same_file in &same_files {
        let dst_filename = &same_file.1[0];
        info!("Working on {}", dst_filename);

        let missing_pieces = get_missing_pieces(&dst_torrent, dst_filename);
        debug!(
            "{} missing_pieces: {:?}",
            missing_pieces.len(),
            &missing_pieces
        );

        'missing_pieces_loop: for &missing_piece_idx in &missing_pieces[0..] {
            let dst_piece = TorrentPiece {
                idx: missing_piece_idx,
                piece_size: dst_torrent.properties.piece_size.unwrap() as u64,
            };
            debug!("Working on missing piece: {:?}", dst_piece);

            let missing_hash = dst_torrent.pieces_hashes[dst_piece.idx];

            let (filename, dst_file_block) =
                piece_to_file_block(&dst_torrent, &Piece::TorrentPiece(dst_piece)).unwrap();
            debug!("filename: {}, fileblock: {:?}", &filename, &dst_file_block);

            let src_filename = match convert_filename(&same_files, &filename) {
                Ok(x) => x,
                Err(_) => continue,
            };
            debug!("dst/src filenames: {} / {}", &filename, &src_filename);
            let src_pieces =
                file_block_to_pieces(&src_torrent, &src_filename, &dst_file_block).unwrap();
            debug!("src_pieces: {:?}", &src_pieces);

            for src_piece in &src_pieces {
                let src_piece_is_available = src_torrent.piece_is_downloaded(src_piece);
                if !src_piece_is_available {
                    debug!("Skipping unavailable piece: {:?}", src_piece);
                    unavailable_pieces += 1;
                    continue 'missing_pieces_loop;
                }
            }

            let mut src_f = get_read_file(&preferences, &src_torrent.properties, &src_filename)
                .unwrap_or_else(|_| panic!("Can't open file {:?}", &src_filename));
            let virt_src_piece = TorrentPiece::merge(&src_pieces).unwrap();
            debug!("virt_src_piece: {:?}", virt_src_piece);
            let (_src_filename, virt_src_file_block) =
                piece_to_file_block(&src_torrent, &Piece::VirtualPiece(virt_src_piece)).unwrap();
            debug!("virt_src_file_block: {:?}", virt_src_file_block);

            if virt_src_file_block.contains(&dst_file_block) {
                // OK!
            } else {
                error!("Can't get data outside file block");
                error!("Can't get data outside file block");
                data_outside_file_block += 1;
                continue 'missing_pieces_loop;
            }

            let data = match read_piece(&mut src_f, virt_src_file_block) {
                Ok(data) => data,
                Err(e) => {
                    error!("Failed to read piece: {:?}", e);
                    unavailable_pieces += 1;
                    continue 'missing_pieces_loop;
                }
            };
            let data_offset = (dst_file_block.offset - virt_src_file_block.offset) as usize; // is positive
            let data = &data[data_offset..(data_offset + dst_file_block.size as usize)];
            let computed_hash = get_sha1(data);

            if computed_hash == missing_hash {
                debug!("hashes match!");
                debug!("Writing to {}", dst_filename);
                let mut dst_f = get_write_file(&preferences, &dst_torrent.properties, dst_filename)
                    .unwrap_or_else(|_| panic!("Can't open file {:?}", &dst_filename));
                write_piece(&mut dst_f, dst_file_block, data).expect("Unable to write file");
                restored_pieces += 1;
            } else {
                warn!("hashes don't match");
            }
        }
    }

    info!("Restored pieces: {}", restored_pieces);
    info!("Unavailable pieces: {}", unavailable_pieces);
    info!("Data outside file block: {}", data_outside_file_block);

    info!("Please recheck torrents!");
    Ok(())
}

async fn work(config_path: Option<&str>, hashes: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(config_path)?;
    let credential = Credential::new(&config.username, &config.password);
    let api = Qbit::new(config.host.as_str(), credential);

    let version = api.get_version().await?;
    info!("qBittorrent version: {}", version);

    for src_hash in hashes.iter() {
        for dst_hash in hashes.iter() {
            if src_hash != dst_hash {
                merge_torrents(&api, src_hash, dst_hash).await?;
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<_> = std::env::args().collect();
    if args.len() < 3 {
        error!("Usage: {} <complete hash> <incomplete hash> [config.json]", args[0]);
        std::process::exit(1);
    }
    
    let config_path = args.get(3).map(String::as_str);
    let hashes = args[1..3].to_vec();

    work(config_path, &hashes).await.unwrap();
}
