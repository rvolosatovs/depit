use crate::{
    copy_wits, untar, Cache, Digest, DigestReader, Identifier, Lock, LockEntry, LockEntrySource,
};

use core::convert::identity;
use core::fmt;
use core::ops::Deref;
use core::str::FromStr;

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context as _};
use async_compression::futures::bufread::GzipDecoder;
use futures::io::BufReader;
use futures::lock::Mutex;
use futures::{stream, AsyncWriteExt, StreamExt, TryStreamExt};
use hex::FromHex;
use serde::{de, Deserialize};
use tokio::fs;
use tracing::{debug, error, info, instrument, warn};
use url::Url;

/// WIT dependency [Manifest] entry
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Entry {
    /// Dependency specification expressed as a resource (typically, a gzipped tarball) URL
    Url {
        /// Resource URL
        url: Url,
        /// Optional sha256 digest of this resource
        sha256: Option<[u8; 32]>,
        /// Optional sha512 digest of this resource
        sha512: Option<[u8; 64]>,
    },
    /// Dependency specification expressed as a local path to a directory containing WIT
    /// definitions
    Path(PathBuf),
    // TODO: Support semver queries
}

impl From<Url> for Entry {
    fn from(url: Url) -> Self {
        Self::Url {
            url,
            sha256: None,
            sha512: None,
        }
    }
}

impl From<PathBuf> for Entry {
    fn from(path: PathBuf) -> Self {
        Self::Path(path)
    }
}

impl FromStr for Entry {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.parse().ok().filter(|url: &Url| !url.cannot_be_a_base()) {
            Some(url) => Ok(Self::from(url)),
            None => Ok(Self::from(PathBuf::from(s))),
        }
    }
}

impl<'de> Deserialize<'de> for Entry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        const FIELDS: [&str; 4] = ["path", "sha256", "sha512", "url"];

        struct Visitor;
        impl<'de> de::Visitor<'de> for Visitor {
            type Value = Entry;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a WIT dependency manifest entry")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                value.parse().map_err(de::Error::custom)
            }

            fn visit_map<V>(self, mut map: V) -> Result<Self::Value, V::Error>
            where
                V: de::MapAccess<'de>,
            {
                let mut path = None;
                let mut sha256 = None;
                let mut sha512 = None;
                let mut url = None;
                while let Some((k, v)) = map.next_entry::<String, String>()? {
                    match k.as_ref() {
                        "path" => {
                            if path.is_some() {
                                return Err(de::Error::duplicate_field("path"));
                            }
                            path = v.parse().map(Some).map_err(|e| {
                                de::Error::custom(format!("invalid `path` field value: {e}"))
                            })?;
                        }
                        "sha256" => {
                            if sha256.is_some() {
                                return Err(de::Error::duplicate_field("sha256"));
                            }
                            sha256 = FromHex::from_hex(v).map(Some).map_err(|e| {
                                de::Error::custom(format!("invalid `sha256` field value: {e}"))
                            })?;
                        }
                        "sha512" => {
                            if sha512.is_some() {
                                return Err(de::Error::duplicate_field("sha512"));
                            }
                            sha512 = FromHex::from_hex(v).map(Some).map_err(|e| {
                                de::Error::custom(format!("invalid `sha512` field value: {e}"))
                            })?;
                        }
                        "url" => {
                            if url.is_some() {
                                return Err(de::Error::duplicate_field("url"));
                            }
                            url = v.parse().map(Some).map_err(|e| {
                                de::Error::custom(format!("invalid `url` field value: {e}"))
                            })?;
                        }
                        k => return Err(de::Error::unknown_field(k, &FIELDS)),
                    }
                }
                match (path, sha256, sha512, url) {
                    (Some(path), None, None, None) => Ok(Entry::Path(path)),
                    (None, sha256, sha512, Some(url)) => Ok(Entry::Url {
                        url,
                        sha256,
                        sha512,
                    }),
                    (Some(_), None | Some(_), None | Some(_), None) => Err(de::Error::custom(
                        "`sha256` and `sha512` are not supported in combination with `path`",
                    )),
                    _ => Err(de::Error::custom("eiter `url` or `path` must be specified")),
                }
            }
        }
        deserializer.deserialize_struct("Entry", &FIELDS, Visitor)
    }
}

fn source_matches(
    digest: impl Into<Digest>,
    sha256: Option<[u8; 32]>,
    sha512: Option<[u8; 64]>,
) -> bool {
    let digest = digest.into();
    sha256.map_or(true, |sha256| sha256 == digest.sha256)
        && sha512.map_or(true, |sha512| sha512 == digest.sha512)
}

impl Entry {
    #[instrument(level = "trace", skip(at, out, lock, cache))]
    async fn lock(
        self,
        at: Option<impl AsRef<Path>>,
        out: impl AsRef<Path>,
        lock: Option<&LockEntry>,
        cache: Option<&impl Cache>,
    ) -> anyhow::Result<LockEntry> {
        let out = out.as_ref();

        match self {
            Self::Path(path) => {
                let dst = at.map(|at| at.as_ref().join(&path));
                copy_wits(&dst.as_ref().unwrap_or(&path), out).await?;
                let digest = LockEntry::digest(dst.as_ref().unwrap_or(&path)).await?;
                Ok(LockEntry::new(LockEntrySource::Path(path), digest))
            }
            Self::Url {
                url,
                sha256,
                sha512,
            } => {
                if let Some(LockEntry {
                    source,
                    digest: lock_digest,
                }) = lock
                {
                    match (LockEntry::digest(out).await, source) {
                        (Ok(digest), LockEntrySource::Url(lock_url))
                            if digest == *lock_digest && url == *lock_url =>
                        {
                            debug!("`{}` is already up-to-date, skip fetch", out.display());
                            return Ok(LockEntry::new(LockEntrySource::Url(url), digest));
                        }
                        (Ok(digest), _) => {
                            debug!(
                                "`{}` is out-of-date (sha256: {})",
                                out.display(),
                                hex::encode(digest.sha256)
                            );
                        }
                        (Err(e), _) if e.kind() == std::io::ErrorKind::NotFound => {
                            debug!("locked dependency for `{url}` missing");
                        }
                        (Err(e), _) => {
                            error!("failed to compute dependency digest for `{url}`: {e}");
                        }
                    }
                }
                let cache = if let Some(cache) = cache {
                    match cache.get(&url).await {
                        Err(e) => error!("failed to get `{url}` from cache: {e}"),
                        Ok(None) => debug!("`{url}` not present in cache"),
                        Ok(Some(tar_gz)) => {
                            let mut hashed = DigestReader::from(tar_gz);
                            match untar(GzipDecoder::new(BufReader::new(&mut hashed)), out).await {
                                Ok(()) if source_matches(hashed, sha256, sha512) => {
                                    debug!("unpacked `{url}` from cache");
                                    return LockEntry::from_url(url, out).await;
                                }
                                Ok(()) => {
                                    warn!("cache hash mismatch for `{url}`");
                                    fs::remove_dir_all(out).await.with_context(|| {
                                        format!("failed to remove `{}`", out.display())
                                    })?;
                                }
                                Err(e) => {
                                    error!("failed to unpack `{url}` contents from cache: {e}");
                                }
                            }
                        }
                    }
                    if let Ok(cache) = cache.insert(&url).await {
                        Some(cache)
                    } else {
                        None
                    }
                } else {
                    None
                };
                let cache = Arc::new(Mutex::new(cache));
                let digest = match url.scheme() {
                    "http" | "https" => {
                        info!("fetch `{url}` into `{}`", out.display());
                        let res = reqwest::get(url.clone())
                            .await
                            .context("failed to GET")
                            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?
                            .error_for_status()
                            .context("GET request failed")
                            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
                        let tar_gz = res
                            .bytes_stream()
                            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
                            .then(|chunk| async {
                                let chunk = chunk?;
                                let mut cache = cache.lock().await;
                                let cache_res = if let Some(w) = cache.as_mut().map(|w| async {
                                    if let Err(e) = w.write(&chunk).await {
                                        error!("failed to write chunk to cache: {e}");
                                        if let Err(e) = w.close().await {
                                            error!("failed to close cache writer: {e}");
                                        }
                                        return Err(e);
                                    }
                                    Ok(())
                                }) {
                                    Some(w.await)
                                } else {
                                    None
                                }
                                .transpose();
                                if cache_res.is_err() {
                                    // Drop the cache writer if a failure occurs
                                    cache.take();
                                }
                                Ok(chunk)
                            })
                            .into_async_read();
                        let mut hashed = DigestReader::from(Box::pin(tar_gz));
                        untar(GzipDecoder::new(BufReader::new(&mut hashed)), out)
                            .await
                            .with_context(|| format!("failed to unpack contents of `{url}`"))?;
                        Digest::from(hashed)
                    }
                    "file" => bail!(
                        r#"`file` scheme is not supported for `url` field, use `path` instead. Try:

```
mydep = "/path/to/my/dep"
```

or

```
[mydep]
path = "/path/to/my/dep"
```
)"#
                    ),
                    scheme => bail!("unsupported URL scheme `{scheme}`"),
                };
                if let Some(sha256) = sha256 {
                    if digest.sha256 != sha256 {
                        fs::remove_dir_all(out)
                            .await
                            .with_context(|| format!("failed to remove `{}`", out.display()))?;
                        bail!(
                            r#"sha256 hash mismatch for `{url}`
got: {}
expected: {}"#,
                            hex::encode(digest.sha256),
                            hex::encode(sha256),
                        );
                    }
                }
                if let Some(sha512) = sha512 {
                    if digest.sha512 != sha512 {
                        fs::remove_dir_all(out)
                            .await
                            .with_context(|| format!("failed to remove `{}`", out.display()))?;
                        bail!(
                            r#"sha512 hash mismatch for `{url}`
got: {}
expected: {}"#,
                            hex::encode(digest.sha512),
                            hex::encode(sha512),
                        );
                    }
                }
                LockEntry::from_url(url, out).await
            }
        }
    }
}

/// WIT dependency manifest mapping [Identifiers](Identifier) to [Entries](Entry)
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct Manifest(HashMap<Identifier, Entry>);

impl Manifest {
    /// Lock the manifest populating `deps`
    #[instrument(level = "trace", skip(at, deps, lock, cache, packages))]
    pub async fn lock(
        self,
        at: Option<impl AsRef<Path>>,
        deps: impl AsRef<Path>,
        lock: Option<&Lock>,
        cache: Option<&impl Cache>,
        packages: impl IntoIterator<Item = &Identifier>,
    ) -> anyhow::Result<Lock> {
        let at = at.as_ref();
        let deps = deps.as_ref();
        let packages: HashSet<_> = packages.into_iter().collect();
        if let Some(id) = packages.iter().find(|id| !self.contains_key(**id)) {
            bail!("selected package `{id}` not found in manifest")
        }
        stream::iter(self.0.into_iter().map(|(id, entry)| async {
            if packages.is_empty() || packages.contains(&id) {
                let out = deps.join(&id);
                let lock = lock.and_then(|lock| lock.get(&id));
                let entry = entry
                    .lock(at, out, lock, cache)
                    .await
                    .with_context(|| format!("failed to lock `{id}`"))?;
                Ok((id, entry))
            } else if let Some(Some(entry)) = lock.map(|lock| lock.get(&id)) {
                Ok((id, entry.clone()))
            } else {
                debug!("locking unselected manifest package `{id}` missing from lock");
                let entry = entry
                    .lock(at, deps.join(&id), None, cache)
                    .await
                    .with_context(|| format!("failed to lock `{id}`"))?;
                Ok((id, entry))
            }
        }))
        .then(identity)
        .try_collect()
        .await
    }
}

impl Deref for Manifest {
    type Target = HashMap<Identifier, Entry>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl FromIterator<(Identifier, Entry)> for Manifest {
    fn from_iter<T: IntoIterator<Item = (Identifier, Entry)>>(iter: T) -> Self {
        Self(HashMap::from_iter(iter))
    }
}

impl<const N: usize> From<[(Identifier, Entry); N]> for Manifest {
    fn from(entries: [(Identifier, Entry); N]) -> Self {
        Self::from_iter(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOO_URL: &str = "https://example.com/foo.tar.gz";

    const BAR_URL: &str = "https://example.com/bar";
    const BAR_SHA256: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    const BAZ_URL: &str = "http://127.0.0.1/baz";
    const BAZ_SHA256: &str = "9f86d081884c7d658a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
    const BAZ_SHA512: &str = "ee26b0dd4af7e749aa1a8ee3c10ae9923f618980772e473f8819a5d4940e0db27ac185f8a0e1d5f84f88bc887fd67b143732c304cc5fa9ad8e6f57f50028a8ff";

    #[test]
    fn decode_url() -> anyhow::Result<()> {
        let manifest: Manifest = toml::from_str(&format!(
            r#"
foo = "{FOO_URL}"
bar = {{ url = "{BAR_URL}", sha256 = "{BAR_SHA256}" }}
baz = {{ url = "{BAZ_URL}", sha256 = "{BAZ_SHA256}", sha512 = "{BAZ_SHA512}" }}
"#
        ))
        .context("failed to decode manifest")?;
        assert_eq!(
            manifest,
            Manifest::from([
                (
                    "foo".parse().expect("failed to parse `foo` identifier"),
                    Entry::Url {
                        url: FOO_URL.parse().expect("failed to parse `foo` URL string"),
                        sha256: None,
                        sha512: None,
                    },
                ),
                (
                    "bar".parse().expect("failed to parse `bar` identifier"),
                    Entry::Url {
                        url: BAR_URL.parse().expect("failed to parse `bar` URL"),
                        sha256: FromHex::from_hex(BAR_SHA256)
                            .map(Some)
                            .expect("failed to decode `bar` sha256"),
                        sha512: None,
                    }
                ),
                (
                    "baz".parse().expect("failed to `baz` parse identifier"),
                    Entry::Url {
                        url: BAZ_URL.parse().expect("failed to parse `baz` URL"),
                        sha256: FromHex::from_hex(BAZ_SHA256)
                            .map(Some)
                            .expect("failed to decode `baz` sha256"),
                        sha512: FromHex::from_hex(BAZ_SHA512)
                            .map(Some)
                            .expect("failed to decode `baz` sha512")
                    }
                )
            ])
        );
        Ok(())
    }

    #[test]
    fn decode_path() -> anyhow::Result<()> {
        let manifest: Manifest = toml::from_str(
            r#"
foo = "/path/to/foo"
bar = { path = "./path/to/bar" }
"#,
        )
        .context("failed to decode manifest")?;
        assert_eq!(
            manifest,
            Manifest::from([
                (
                    "foo".parse().expect("failed to parse `foo` identifier"),
                    Entry::Path(PathBuf::from("/path/to/foo")),
                ),
                (
                    "bar".parse().expect("failed to parse `bar` identifier"),
                    Entry::Path(PathBuf::from("./path/to/bar")),
                ),
            ])
        );
        Ok(())
    }
}
