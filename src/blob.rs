//! Reads one blob's bytes in order, so that a file of any size is sent without being held in
//! memory. gix reads an object whole, so this reads the two forms Git stores a large blob in
//! itself: a loose object, and a pack entry that is not a delta. Both are one zlib stream in a
//! file. A blob stored as a delta can only be rebuilt whole, and is read with gix.

use std::fs::{self, File};
use std::io::{self, BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, bail};
use gix::ObjectId;
use gix::odb::pack;
use gix::zlib::Decompress;
use gix::zlib::stream::inflate;

/// How much of a file is read at a time.
const BUFFER: usize = 64 << 10;
/// The longest header of a loose object that is read: `blob`, a space, a size and a NUL.
const MAX_LOOSE_HEADER: usize = 32;

/// A blob being read from its first byte to its last.
pub struct Reader {
    source: Source,
    size: u64,
    /// How many bytes are still to come.
    left: u64,
}

enum Source {
    Deflated(Stream),
    /// The whole blob, rebuilt from its deltas.
    Whole(Cursor<Vec<u8>>),
}

/// A zlib stream in a file, read up to where a blob's bytes start.
struct Stream {
    file: BufReader<File>,
    inflate: Box<Decompress>,
    /// The blob's size, from the header before the stream or at its start.
    size: u64,
}

impl Reader {
    /// Opens the blob `id` of `repo`: the loose object when there is one, or else its entry in
    /// a pack. Whatever cannot be read as a stream, such as a delta or a blob in an alternate
    /// object directory, is read whole.
    ///
    /// # Errors
    ///
    /// Fails when the blob cannot be found or read.
    pub(crate) fn open(repo: &gix::Repository, id: ObjectId) -> Result<Self> {
        let objects = repo.objects.store_ref().path();
        let stream = loose(objects, id)
            .transpose()
            .or_else(|| packed(objects, id).transpose());
        // When neither can be read, Git may be writing or repacking right now, which gix knows
        // how to wait out.
        let (source, size) = if let Some(Ok(stream)) = stream {
            let size = stream.size;
            (Source::Deflated(stream), size)
        } else {
            let data = repo
                .find_blob(id)
                .with_context(|| format!("reading blob {id}"))?
                .detach()
                .data;
            let size = data.len() as u64;
            (Source::Whole(Cursor::new(data)), size)
        };
        Ok(Self {
            source,
            size,
            left: size,
        })
    }

    /// The blob's size in bytes.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Reads and drops the next `count` bytes. A zlib stream has no way to start in its middle.
    ///
    /// # Errors
    ///
    /// Fails when the blob cannot be read or ends before `count` bytes.
    pub fn skip(&mut self, count: u64) -> io::Result<()> {
        let skipped = io::copy(&mut self.by_ref().take(count), &mut io::sink())?;
        if skipped == count {
            Ok(())
        } else {
            Err(io::ErrorKind::UnexpectedEof.into())
        }
    }
}

impl Read for Reader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let most = usize::try_from(self.left).map_or(buffer.len(), |left| left.min(buffer.len()));
        if most == 0 {
            return Ok(0);
        }
        let read = match &mut self.source {
            Source::Deflated(stream) => {
                inflate::read(&mut stream.file, &mut stream.inflate, &mut buffer[..most])?
            }
            Source::Whole(data) => data.read(&mut buffer[..most])?,
        };
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the blob ends before its size",
            ));
        }
        self.left -= read as u64;
        Ok(read)
    }
}

/// The loose object of `id`, read past its header, which is the start of its zlib stream.
fn loose(objects: &Path, id: ObjectId) -> Result<Option<Stream>> {
    let hex = id.to_string();
    let (folder, name) = hex.split_at(2);
    let file = match File::open(objects.join(folder).join(name)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut file = BufReader::with_capacity(BUFFER, file);
    let mut inflate = Box::new(Decompress::default());
    let mut header = Vec::with_capacity(MAX_LOOSE_HEADER);
    let mut byte = [0];
    loop {
        if inflate::read(&mut file, &mut inflate, &mut byte)? == 0
            || header.len() == MAX_LOOSE_HEADER
        {
            bail!("the loose object {id} has no header");
        }
        if byte[0] == 0 {
            break;
        }
        header.push(byte[0]);
    }
    let size = std::str::from_utf8(&header)
        .ok()
        .and_then(|header| header.strip_prefix("blob "))
        .and_then(|size| size.parse().ok())
        .with_context(|| format!("the loose object {id} is not a blob"))?;
    Ok(Some(Stream {
        file,
        inflate,
        size,
    }))
}

/// The entry of `id` in a pack, read past its header, when a pack stores it whole. `None` when
/// no pack has it, or every pack that has it stores it as a delta.
fn packed(objects: &Path, id: ObjectId) -> Result<Option<Stream>> {
    let entries = match fs::read_dir(objects.join("pack")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|extension| extension != "idx") {
            continue;
        }
        let index = pack::index::File::at(&path, id.kind())?;
        let Some(found) = index.lookup(id) else {
            continue;
        };
        let offset = index.pack_offset_at_index(found);
        let mut file = BufReader::with_capacity(BUFFER, File::open(path.with_extension("pack"))?);
        file.seek(SeekFrom::Start(offset))?;
        let entry = pack::data::Entry::from_read(&mut file, offset, id.kind().len_in_bytes())?;
        // Another pack may hold the same blob whole.
        if matches!(entry.header, pack::data::entry::Header::Blob) {
            return Ok(Some(Stream {
                file,
                inflate: Box::new(Decompress::default()),
                size: entry.decompressed_size,
            }));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    /// Runs `git` in `dir` with no configuration of the machine's own, and returns what it
    /// printed.
    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("HOME", dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn read(stream: Stream) -> Vec<u8> {
        let size = stream.size;
        let mut reader = Reader {
            source: Source::Deflated(stream),
            size,
            left: size,
        };
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn a_blob_is_read_as_a_stream_from_a_loose_object_and_from_a_pack() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut rng = fastrand::Rng::with_seed(7);
        let bytes: Vec<u8> = std::iter::repeat_with(|| rng.u8(..))
            .take(3 * BUFFER + 17)
            .collect();
        std::fs::write(dir.path().join("film.bin"), &bytes).unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        let id = git(dir.path(), &["hash-object", "-w", "film.bin"]);
        let id = ObjectId::from_hex(id.as_bytes()).unwrap();
        let objects = dir.path().join(".git/objects");

        let stream = loose(&objects, id).unwrap().expect("a loose object");
        assert_eq!(stream.size, bytes.len() as u64);
        assert!(read(stream) == bytes, "the loose object's bytes");
        assert!(packed(&objects, id).unwrap().is_none(), "no pack yet");

        git(dir.path(), &["add", "film.bin"]);
        git(
            dir.path(),
            &[
                "-c",
                "user.name=Writer",
                "-c",
                "user.email=writer@example.invalid",
                "commit",
                "-q",
                "-m",
                "Add a film",
            ],
        );
        git(dir.path(), &["repack", "-q", "-a", "-d"]);
        assert!(loose(&objects, id).unwrap().is_none(), "packed and pruned");
        let stream = packed(&objects, id).unwrap().expect("a pack entry");
        assert_eq!(stream.size, bytes.len() as u64);
        assert!(read(stream) == bytes, "the pack entry's bytes");

        let repo = gix::open(dir.path()).unwrap();
        let mut reader = Reader::open(&repo, id).unwrap();
        assert!(matches!(reader.source, Source::Deflated(_)));
        reader.skip(BUFFER as u64 + 5).unwrap();
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert!(rest == bytes[BUFFER + 5..], "the bytes after a skip");
        assert!(
            Reader::open(&repo, id)
                .unwrap()
                .skip(bytes.len() as u64 + 1)
                .is_err(),
            "a skip past the end fails"
        );
    }
}
