extern crate rollsum;
extern crate crypto;
#[macro_use]
extern crate log;
extern crate rustc_serialize as serialize;
extern crate argparse;
extern crate sodiumoxide;
extern crate flate2;
#[cfg(test)]
extern crate rand;

use std::io::{Read, Write};
use std::{fs, mem, thread, io};
use std::path::{Path, PathBuf};
use serialize::hex::{ToHex, FromHex};
use std::collections::HashSet;

use std::sync::mpsc;
use std::cell::RefCell;

use rollsum::Engine;
use crypto::sha2;
use crypto::digest::Digest;

use sodiumoxide::crypto::box_;

mod error;

use error::{Result, Error};

macro_rules! printerrln {
    ($($arg:tt)*) => ({
        use std::io::prelude::*;
        if let Err(e) = writeln!(&mut ::std::io::stderr(), "{}\n", format_args!($($arg)*)) {
            panic!("Failed to write to stderr.\nOriginal error output: {}\nSecondary error writing to stderr: {}", format!($($arg)*), e);
        }
    })
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ChunkType {
    Index,
    Data,
}

impl ChunkType {
    fn should_compress(&self) -> bool {
        *self == ChunkType::Data
    }

    fn should_encrypt(&self) -> bool {
        *self == ChunkType::Data
    }
}

enum ChunkWriterMessage {
    // ChunkType in every Data is somewhat redundant...
    Data(Vec<u8>, Vec<Edge>, ChunkType, ChunkType),
    Exit,
}

/// Edge: offset in the input and sha256 sum of the chunk
type Edge = (usize, Vec<u8>);

struct Chunker {
    roll : rollsum::Bup,
    sha256 : sha2::Sha256,
    bytes_total : usize,
    bytes_chunk: usize,
    chunks_total : usize,

    edges : Vec<Edge>,
}

impl Chunker {
    pub fn new() -> Self {
        Chunker {
            roll: rollsum::Bup::new(),
            sha256: sha2::Sha256::new(),
            bytes_total: 0,
            bytes_chunk: 0,
            chunks_total: 0,
            edges: vec!(),
        }
    }

    pub fn edge_found(&mut self, input_ofs : usize) {
        debug!("found edge at {}; sum: {:x}",
                 self.bytes_total,
                 self.roll.digest());

        debug!("sha256 hash: {}",
                 self.sha256.result_str());

        let mut sha256 = vec![0u8; 32];
        self.sha256.result(&mut sha256);

        self.edges.push((input_ofs, sha256));

        self.chunks_total += 1;
        self.bytes_chunk += 0;

        self.sha256.reset();
        self.roll = rollsum::Bup::new();
    }

    pub fn input(&mut self, buf : &[u8]) -> Vec<Edge> {
        let mut ofs : usize = 0;
        let len = buf.len();
        while ofs < len {
            if let Some(count) = self.roll.find_chunk_edge(&buf[ofs..len]) {
                self.sha256.input(&buf[ofs..ofs+count]);

                ofs += count;

                self.bytes_chunk += count;
                self.bytes_total += count;
                self.edge_found(ofs);
            } else {
                let count = len - ofs;
                self.sha256.input(&buf[ofs..len]);
                self.bytes_chunk += count;
                self.bytes_total += count;
                break
            }
        }
        mem::replace(&mut self.edges, vec!())
    }

    pub fn finish(&mut self) -> Vec<Edge> {
        if self.bytes_chunk != 0 || self.bytes_total == 0 {
            self.edge_found(0);
        }
        mem::replace(&mut self.edges, vec!())
    }
}
fn quick_sha256(data : &[u8]) -> Vec<u8> {

    let mut sha256 = sha2::Sha256::new();
    sha256.input(&data);
    let mut sha256_digest = vec![0u8; 32];
    sha256.result(&mut sha256_digest);

    return sha256_digest
}

/// Store data, using input_f to get chunks of data
///
/// Return final digest
fn chunk_and_send_to_writer<R : Read>(tx : &mpsc::Sender<ChunkWriterMessage>,
                      mut reader : &mut R,
                      data_type : ChunkType,
                      ) -> Result<Vec<u8>> {
    let mut chunker = Chunker::new();

    let mut index : Vec<u8> = vec!();
    loop {
        let mut buf = vec![0u8; 16 * 1024];
        let len = try!(reader.read(&mut buf));

        if len == 0 {
            break;
        }
        buf.truncate(len);

        let edges = chunker.input(&buf[..len]);

        for &(_, ref sum) in &edges {
            index.append(&mut sum.clone());
        }
        tx.send(ChunkWriterMessage::Data(buf, edges, ChunkType::Data, data_type)).unwrap();
    }
    let edges = chunker.finish();

    for &(_, ref sum) in &edges {
        index.append(&mut sum.clone());
    }
    tx.send(ChunkWriterMessage::Data(vec!(), edges, ChunkType::Data, data_type)).unwrap();

    if index.len() > 32 {
        let digest = try!(chunk_and_send_to_writer(tx, &mut io::Cursor::new(index), ChunkType::Index));
        assert!(digest.len() == 32);
        let index_digest = quick_sha256(&digest);
        printerrln!("{} -> {}", index_digest.to_hex(), digest.to_hex());
        tx.send(ChunkWriterMessage::Data(digest.clone(), vec![(digest.len(), index_digest.clone())], ChunkType::Index, ChunkType::Index)).unwrap();
        Ok(index_digest)
    } else {
        Ok(index)
    }
}


fn pub_key_file_path(path : &Path) -> PathBuf {
    path.join("pub_key")
}

pub struct SecretKey(box_::SecretKey);

impl SecretKey {
    pub fn from_str(s : &str) -> Option<Self> {
        s.from_hex().ok()
            .and_then(|bytes| box_::SecretKey::from_slice(&bytes))
            .map(|sk| SecretKey(sk))
    }

    pub fn to_string(&self) -> String {
        (self.0).0.to_hex()
    }
}

// Can be feed Index data, will
// write translated data to `writer`.
struct IndexTranslator<'a> {
    repo : &'a Repo,
    digest : Vec<u8>,
    writer : &'a mut io::Write,
    data_type : ChunkType,
    sec_key : Option<&'a box_::SecretKey>,
}

impl<'a> IndexTranslator<'a> {
    fn new(repo : &'a Repo, writer : &'a mut io::Write, data_type : ChunkType, sec_key : Option<&'a box_::SecretKey>) -> Self {
        IndexTranslator {
            repo: repo,
            writer : writer,
            digest : vec!(),
            data_type : data_type,
            sec_key : sec_key,
        }
    }
}

impl<'a> io::Write for IndexTranslator<'a> {
    fn write(&mut self, mut bytes : &[u8]) -> io::Result<usize> {
        let total_len = bytes.len();
        loop {
            let has_already = self.digest.len();
            if (has_already + bytes.len()) < 32 {
                self.digest.extend_from_slice(bytes);
                return Ok(total_len);
            }

            let needs = 32 - has_already;
            self.digest.extend_from_slice(&bytes[..needs]);
            bytes = &bytes[needs..];
            printerrln!("Translated part of index: {}; dt: {:?}", self.digest.to_hex(), self.data_type);
            try!(self.repo.read_recursively(&self.digest, self.writer, self.data_type, self.sec_key)
                 .map_err(|err| match err {
                     Error::Io(io_err) => io_err,
                     _ => io::Error::new(io::ErrorKind::Other, "Error while traversing index data"),
                 }));
            self.digest.clear();
        }
    }

    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}
#[derive(Clone, Debug)]
pub struct Repo {
    path : PathBuf,
    pub_key : box_::PublicKey,
}

impl Repo {
    pub fn init(repo_path : &Path) -> Result<(Repo, SecretKey)> {
        if repo_path.exists() {
            return Err(Error::Exists);
        }

        try!(fs::create_dir_all(&repo_path));
        let pubkey_path = pub_key_file_path(&repo_path);

        let mut pubkey_file = try!(fs::File::create(pubkey_path));
        let (pk, sk) = box_::gen_keypair();

        try!(pubkey_file.write_all(&pk.0.to_hex().as_bytes()));
        try!(pubkey_file.flush());

        let repo = Repo {
            path : repo_path.to_owned(),
            pub_key : pk,
        };
        Ok((repo, SecretKey(sk)))
    }

    pub fn open(repo_path : &Path) -> Result<Repo> {

        if !repo_path.exists() {
            return Err(Error::NotFound);
        }

        let pubkey_path = pub_key_file_path(&repo_path);
        if !pubkey_path.exists() {
            return Err(Error::NotFound);
        }

        let mut file = try!(fs::File::open(&pubkey_path));

        let mut buf = vec!();
        try!(file.read_to_end(&mut buf));

        let pubkey_str = try!(std::str::from_utf8(&buf)
            .map_err(|_| Error::InvalidPubKey));
        let pubkey_bytes = try!(pubkey_str.from_hex().map_err(|_| Error::InvalidPubKey));
        let pub_key = try!(box_::PublicKey::from_slice(&pubkey_bytes).ok_or(Error::InvalidPubKey));

        Ok(Repo {
            path : repo_path.to_owned(),
            pub_key : pub_key,
        })
    }

    /// Accept messages on rx and writes them to chunk files
    fn chunk_writer(&self, rx : mpsc::Receiver<ChunkWriterMessage>) {
        let mut previous_parts = vec!();

        loop {
            match rx.recv().unwrap() {
                ChunkWriterMessage::Exit => {
                    assert!(previous_parts.is_empty());
                    return
                }
                ChunkWriterMessage::Data(part, edges, chunk_type, data_type) => {
                    if edges.is_empty() {
                        previous_parts.push(part)
                    } else {
                        let mut prev_ofs = 0;
                        for &(ref ofs, ref sha256) in &edges {
                            let path = self.chunk_path_by_digest(&sha256, chunk_type);
                            if !path.exists() {
                                fs::create_dir_all(path.parent().unwrap()).unwrap();
                                let mut chunk_file = fs::File::create(path).unwrap();

                                let (ephemeral_pub, ephemeral_sec) = box_::gen_keypair();

                                let mut chunk_data = Vec::with_capacity(16 * 1024);

                                for previous_part in previous_parts.drain(..) {
                                    chunk_data.write_all(&previous_part).unwrap();
                                }
                                if *ofs != prev_ofs {
                                    chunk_data.write_all(&part[prev_ofs..*ofs]).unwrap();
                                }

                                printerrln!("Writing {}; data_type: {:?}", sha256.to_hex(), data_type);

                                let chunk_data = if data_type.should_compress() {
                                    let mut compressor = flate2::write::DeflateEncoder::new(
                                        Vec::with_capacity(chunk_data.len()), flate2::Compression::Default
                                        );

                                    compressor.write_all(&chunk_data).unwrap();
                                    compressor.finish().unwrap()
                                } else {
                                    chunk_data
                                };

                                if data_type.should_encrypt() {
                                    let pub_key = &self.pub_key;
                                    let nonce = box_::Nonce::from_slice(&sha256[0..box_::NONCEBYTES]).unwrap();

                                    let cipher = box_::seal(
                                        &chunk_data,
                                        &nonce,
                                        &pub_key,
                                        &ephemeral_sec
                                        );
                                    chunk_file.write_all(&ephemeral_pub.0).unwrap();
                                    chunk_file.write_all(&cipher).unwrap();
                                } else {
                                    chunk_file.write_all(&chunk_data).unwrap();
                                }
                            } else {
                                previous_parts.clear();
                            }
                            debug_assert!(previous_parts.is_empty());

                            prev_ofs = *ofs;
                        }
                        if prev_ofs != part.len() {
                            let mut part = part;
                            previous_parts.push(part.split_off(prev_ofs))
                        }
                    }
                }
            }
        }
    }

    pub fn write<R : Read>(&self, name : &str, reader : &mut R) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        let self_clone = self.clone();
        let chunk_writer_join = thread::spawn(move || self_clone.chunk_writer(rx));

        let final_digest = try!(chunk_and_send_to_writer(&tx, reader, ChunkType::Data));

        tx.send(ChunkWriterMessage::Exit).unwrap();
        chunk_writer_join.join().unwrap();

        self.store_digest_as_backup_name(&final_digest, name)
    }

    pub fn read<W : Write>(&self, name : &str, writer: &mut W, sec_key : &SecretKey) -> Result<()> {
        let digest = try!(self.name_to_digest(name));

        self.read_recursively(
            &digest,
            writer,
            ChunkType::Data,
            Some(&sec_key.0)
            )
    }

    pub fn name_to_digest(&self, name : &str) -> Result<Vec<u8>> {
        let backup_path = self.path.join("backup").join(name);
        if !backup_path.exists() {
            return Err(Error::NotFound);
        }

        let mut file = try!(fs::File::open(&backup_path));
        let mut buf = vec!();
        try!(file.read_to_end(&mut buf));

        Ok(buf)
    }

    fn store_digest_as_backup_name(&self, digest : &[u8], name : &str) -> Result<()> {
        let backup_dir = self.path.join("backup");
        try!(fs::create_dir_all(&backup_dir));
        let backup_path = backup_dir.join(name);

        if backup_path.exists() {
            return Err(Error::Exists);
        }

        let mut file = try!(fs::File::create(&backup_path));

        try!(file.write_all(digest));
        Ok(())
    }

/*
    pub fn du(&self, name : &str, sec_key : &SecretKey) -> Result<u64> {

        let digest = try!(self.name_to_digest(name));

        self.du_by_digest(&digest, sec_key)
    }


    pub fn du_by_digest(&self, digest : &[u8], sec_key : &SecretKey) -> Result<u64> {
        let mut bytes = 0u64;

        try!(self.traverse_recursively(
            digest,
            &mut Self::traverse_index,
            &mut |repo, digest| {
                let mut data = vec!();
                try!(repo.read_chunk_into(digest, ChunkType::Data, &mut data, Some(&sec_key.0)));
                bytes += data.len() as u64;
                Ok(())
            },
            ));

        Ok(bytes)
    }

    */
    /*
    fn traverse_recursively(
        &self,
        digest : &[u8],
        on_index: &mut FnMut(&Self, &[u8], &mut FnMut(&Self, &[u8]) -> Result<()>) -> Result<()>,
        on_data: &mut FnMut(&Self, &[u8]) -> Result<()>,
        ) -> Result<()> {

        let chunk_type = try!(self.chunk_type(digest));

        match chunk_type {
            ChunkType::Index => {
                try!(on_index(self, digest, on_data));
            },
            ChunkType::Data => {
                try!(on_data(self, digest));
            },
        }
        Ok(())
    }
    */


    fn read_recursively(
        &self,
        digest : &[u8],
        writer : &mut io::Write,
        data_type : ChunkType,
        sec_key : Option<&box_::SecretKey>
        ) -> Result<()> {

        let chunk_type = try!(self.chunk_type(digest));

        match chunk_type {
            ChunkType::Index => {
                let mut index_data = vec!();
                printerrln!("RR I: {}", digest.to_hex());
                try!(self.read_chunk_into(digest, ChunkType::Index, ChunkType::Index, &mut index_data, sec_key));

                assert!(index_data.len() == 32);

                let mut translator = IndexTranslator::new(self, writer, data_type, sec_key);
                try!(self.read_recursively(&index_data, &mut translator, ChunkType::Index,  None))
            },
            ChunkType::Data => {
                printerrln!("RR D: {}", digest.to_hex());
                try!(self.read_chunk_into(digest, ChunkType::Data, data_type, writer, sec_key))
            },
        }
        Ok(())
    }

/*
    fn traverse_index(&self, digest : &[u8], on_data : &mut FnMut(&Repo, &[u8]) -> Result<()>) -> Result<()> {
        let mut index_data = vec!();

        assert!(self.chunk_type(digest) == ChunkType::Index);
        try!(self.read_chunk_into(digest, ChunkType::Index, &mut index_data, None));

        assert!(index_data.len() % 32 == 0);

        let _ = index_data.chunks(32).map(|slice| {
            self.traverse_recursively(slice, &mut Self::traverse_index, on_data)
        }).count();

        Ok(())
    }
*/
    /*
    fn reachable_recursively_insert(&self,
                               digest : &[u8],
                               reachable_digests : &RefCell<HashSet<Vec<u8>>>,
                               ) -> Result<()> {
        reachable_digests.borrow_mut().insert(digest.to_owned());

        self.traverse_recursively(
            digest,
            &mut |repo, digest, on_data| {
                reachable_digests.borrow_mut().insert(digest.to_owned());
                repo.traverse_index(digest, on_data)
            },
            &mut |_, digest| {
                reachable_digests.borrow_mut().insert(digest.to_owned());
                Ok(())
            },
            )
    }
    */

    /// List all backups
    pub fn list_names(&self) -> Result<Vec<String>> {
        let mut ret : Vec<String> = vec!();

        let backup_dir = self.path.join("backup");
        for entry in try!(fs::read_dir(backup_dir)) {
            let entry = try!(entry);
            let name = entry.file_name().to_string_lossy().to_string();
            ret.push(name)
        }

        Ok(ret)
    }

    pub fn list_stored_chunks(&self) -> Result<HashSet<Vec<u8>>> {
        fn insert_all_digest(path : &Path, reachable : &mut HashSet<Vec<u8>>) {
            for out_entry in fs::read_dir(path).unwrap() {
                let out_entry = out_entry.unwrap();
                for mid_entry in fs::read_dir(out_entry.path()).unwrap() {
                    let mid_entry = mid_entry.unwrap();
                    for entry in fs::read_dir(mid_entry.path()).unwrap() {
                        let entry = entry.unwrap();
                        let name= entry.file_name().to_string_lossy().to_string();
                        let entry_digest = name.from_hex().unwrap();
                        reachable.insert(entry_digest);
                    }
                }
            }
        }

        let mut digests = HashSet::new();
        insert_all_digest(&self.path.join("index"), &mut digests);
        insert_all_digest(&self.path.join("chunks"), &mut digests);
        Ok(digests)
    }

/*
    /// Return all reachable chunks
    pub fn list_reachable_chunks(&self) -> Result<HashSet<Vec<u8>>> {
        let reachable_digests = RefCell::new(HashSet::new());
        let all_names = try!(self.list_names());
        for name in &all_names {
            let digest = try!(self.name_to_digest(&name));
            try!(self.reachable_recursively_insert(&digest, &reachable_digests));
        }
        Ok(reachable_digests.into_inner())
    }

*/
    fn chunk_type(&self, digest : &[u8]) -> Result<ChunkType> {
        for i in &[ChunkType::Index, ChunkType::Data] {
            let file_path = self.chunk_path_by_digest(digest, *i);
            if file_path.exists() {
                return Ok(*i)
            }
        }
        Err(Error::NotFound)
    }

    fn chunk_path_by_digest(&self, digest : &[u8], chunk_type : ChunkType) -> PathBuf {
        let i_or_c = match chunk_type {
            ChunkType::Data => Path::new("chunks"),
            ChunkType::Index => Path::new("index"),
        };

        self.path.join(i_or_c)
            .join(&digest[0..1].to_hex())
            .join(digest[1..2].to_hex())
            .join(&digest.to_hex())
    }
/*
    fn read_into(&self,
                        digest : &[u8],
                        writer: &mut Write,
                        sec_key : Option<&box_::SecretKey>,
                        ) -> Result<()> {

        let chunk_type = try!(self.chunk_type(digest));

        match chunk_type {
            ChunkType::Data => read_chunk_into(digest, writer, sec_key),
            ChunkType::Index => {
                let translator = IndexTranslator::new(self, writer);
                try!(read_chunk_into(digest, translator, sec_key))
            }
        }
    }
    */

    fn read_chunk_into(&self,
                        digest : &[u8],
                        chunk_type : ChunkType,
                        data_type : ChunkType,
                        writer: &mut Write,
                        sec_key : Option<&box_::SecretKey>,
                        ) -> Result<()> {
        let path = self.chunk_path_by_digest(digest, chunk_type);
        let mut file = try!(fs::File::open(path));
        let mut data = Vec::with_capacity(16 * 1024);

        let data = if data_type.should_encrypt() {
            let mut ephemeral_pub = [0; box_::PUBLICKEYBYTES];
            try!(file.read_exact(&mut ephemeral_pub));
            try!(file.read_to_end(&mut data));
            let nonce = box_::Nonce::from_slice(&digest[0..box_::NONCEBYTES]).unwrap();
            try!(box_::open(&data, &nonce, &box_::PublicKey(ephemeral_pub), sec_key.unwrap())
                 .map_err(|_| Error::DecryptionFailed)
                 )
        } else {
            try!(file.read_to_end(&mut data));
            data
        };

        let data = if data_type.should_compress() {
            let mut decompressor = flate2::write::DeflateDecoder::new(Vec::with_capacity(data.len()));

            try!(decompressor.write_all(&data));
            try!(decompressor.finish())
        } else {
            data
        };

//        if chunk_type == ChunkType::Data {
            let mut sha256 = sha2::Sha256::new();
            sha256.input(&data);
            let mut sha256_digest = vec![0u8; 32];
            sha256.result(&mut sha256_digest);
            if sha256_digest != digest {
                panic!("{} corrupted, data read: {}", digest.to_hex(), sha256_digest.to_hex());
            }
 //       }
        try!(io::copy(&mut io::Cursor::new(data), writer));
        Ok(())
    }

}


#[cfg(test)]
mod tests;
