use std::{
    fs,
    io::{self, BufWriter, Error, ErrorKind, Write as _},
    path::Path,
};

use super::{decode::read_base64_from_bytes, encode::write_base64_to_bytes};

pub trait Filesystem {
    fn read_file(&mut self, file_name: &str) -> io::Result<Option<Vec<u8>>>;
    fn write_file(&mut self, file_name: &str, contents: &[u8]) -> io::Result<()>;
}

pub struct Disk<P: AsRef<Path>> {
    child_path: P,
}

impl<P: AsRef<Path>> Disk<P> {
    pub fn new(child_path: P) -> Self {
        Self { child_path }
    }
}

impl<P: AsRef<Path>> Filesystem for Disk<P> {
    fn read_file(&mut self, file_name: &str) -> io::Result<Option<Vec<u8>>> {
        let full_path = self.child_path.as_ref().join(format!("{file_name}.js"));
        let raw_input = match fs::read(full_path) {
            Ok(raw_input) => raw_input,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let Some(raw_input) = raw_input
            .strip_prefix(br#"rn_(""#)
            .and_then(|input| input.strip_suffix(br#"")"#))
        else {
            return Err(Error::from(ErrorKind::InvalidData));
        };

        let mut input = Vec::with_capacity(raw_input.len() * 3 / 4);
        read_base64_from_bytes(raw_input, &mut input)
            .map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
        Ok(Some(input))
    }

    fn write_file(&mut self, file_name: &str, contents: &[u8]) -> io::Result<()> {
        let full_path = self.child_path.as_ref().join(format!("{file_name}.js"));
        let mut file =
            BufWriter::with_capacity((contents.len() * 8 / 6) + 7, fs::File::create(full_path)?);
        file.write_all(br#"rn_(""#)?;
        write_base64_to_bytes(contents, &mut file)?;
        file.write_all(br#"")"#)?;
        Ok(())
    }
}
