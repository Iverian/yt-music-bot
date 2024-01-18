use std::io;

use camino::{Utf8Path, Utf8PathBuf};
use rand::Rng;
use tokio::fs;

const PREFIX: &str = "tmp";
const ALPHABET: &[char] = &[
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's',
    't', 'u', 'v', 'w', 'x', 'y', 'z', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L',
    'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z', '0', '1', '2', '3', '4',
    '5', '6', '7', '8', '9', '-',
];
const WORD_LENGTH: usize = 8;

pub struct TempDir(Utf8PathBuf);

impl TempDir {
    pub async fn new(parent: Utf8PathBuf) -> io::Result<Self> {
        Ok(Self(Self::create_dir(parent).await?))
    }

    pub fn path(&self) -> &Utf8Path {
        &self.0
    }

    pub async fn close(self) {
        fs::remove_dir_all(self.0).await.ok();
    }

    async fn create_dir(mut parent: Utf8PathBuf) -> io::Result<Utf8PathBuf> {
        fs::create_dir_all(&parent).await?;
        parent = fs::canonicalize(parent).await?.try_into().unwrap();
        loop {
            let path = parent.join(Self::make_name());
            if let Err(e) = fs::create_dir_all(&path).await {
                match e.kind() {
                    io::ErrorKind::AlreadyExists => continue,
                    _ => return Err(e),
                }
            }
            return Ok(path);
        }
    }

    fn make_name() -> String {
        let mut rng = rand::thread_rng();
        let mut result = PREFIX.to_owned();
        result.extend((0..WORD_LENGTH).map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())]));
        result
    }
}
