// Copyright 2018-2020 the Deno authors. All rights reserved. MIT license.

/// This module is meant to eventually implement HTTP cache
/// as defined in RFC 7234 (https://tools.ietf.org/html/rfc7234).
/// Currently it's a very simplified version to fulfill Deno needs
/// at hand.
use crate::fs as deno_fs;
use crate::http_util::HeadersMap;
use deno_core::ErrBox;
use serde::Serialize;
use serde_derive::Deserialize;
use std::fs;
use std::fs::File;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use url::Url;

/// Turn base of url (scheme, hostname, port) into a valid filename.
/// This method replaces port part with a special string token (because
/// ":" cannot be used in filename on some platforms).
/// Ex: $DENO_DIR/deps/https/deno.land/
fn base_url_to_filename(url: &Url) -> PathBuf {
  let mut out = PathBuf::new();

  let scheme = url.scheme();
  out.push(scheme);

  match scheme {
    "http" | "https" => {
      let host = url.host_str().unwrap();
      let host_port = match url.port() {
        Some(port) => format!("{}_PORT{}", host, port),
        None => host.to_string(),
      };
      out.push(host_port);
    }
    scheme => {
      unimplemented!(
        "Don't know how to create cache name for scheme: {}",
        scheme
      );
    }
  };

  out
}

/// Turn provided `url` into a hashed filename.
/// URLs can contain a lot of characters that cannot be used
/// in filenames (like "?", "#", ":"), so in order to cache
/// them properly they are deterministically hashed into ASCII
/// strings.
///
/// NOTE: this method is `pub` because it's used in integration_tests
pub fn url_to_filename(url: &Url) -> PathBuf {
  let mut cache_filename = base_url_to_filename(url);

  let mut rest_str = url.path().to_string();
  if let Some(query) = url.query() {
    rest_str.push_str("?");
    rest_str.push_str(query);
  }
  // NOTE: fragment is omitted on purpose - it's not taken into
  // account when caching - it denotes parts of webpage, which
  // in case of static resources doesn't make much sense
  let hashed_filename = crate::checksum::gen(vec![rest_str.as_bytes()]);
  cache_filename.push(hashed_filename);
  cache_filename
}

#[derive(Clone)]
pub struct HttpCache {
  pub location: PathBuf,
}

#[derive(Serialize, Deserialize)]
pub struct Metadata {
  pub headers: HeadersMap,
  pub url: String,
}

impl Metadata {
  pub fn write(&self, cache_filename: &Path) -> Result<(), ErrBox> {
    let metadata_filename = Self::filename(cache_filename);
    let json = serde_json::to_string_pretty(self)?;
    deno_fs::write_file(&metadata_filename, json, 0o666)?;
    Ok(())
  }

  pub fn read(cache_filename: &Path) -> Result<Metadata, ErrBox> {
    let metadata_filename = Metadata::filename(&cache_filename);
    let metadata = fs::read_to_string(metadata_filename)?;
    let metadata: Metadata = serde_json::from_str(&metadata)?;
    Ok(metadata)
  }

  /// Ex: $DENO_DIR/deps/https/deno.land/c885b7dcf1d6936e33a9cc3a2d74ec79bab5d733d3701c85a029b7f7ec9fbed4.metadata.json
  pub fn filename(cache_filename: &Path) -> PathBuf {
    cache_filename.with_extension("metadata.json")
  }
}

impl HttpCache {
  /// Returns a new instance.
  pub fn new(location: &Path) -> Self {
    Self {
      location: location.to_owned(),
    }
  }

  /// Ensures the location of the cache.
  pub fn ensure_location(&self) -> io::Result<()> {
    if self.location.is_dir() {
      return Ok(());
    }
    fs::create_dir_all(&self.location).map_err(|e| {
      io::Error::new(
        e.kind(),
        format!(
          "Could not create remote modules cache location: {:?}\nCheck the permission of the directory.",
          self.location
        ),
      )
    })
  }

  pub(crate) fn get_cache_filename(&self, url: &Url) -> PathBuf {
    self.location.join(url_to_filename(url))
  }

  // TODO(bartlomieju): this method should check headers file
  // and validate against ETAG/Last-modified-as headers.
  // ETAG check is currently done in `cli/file_fetcher.rs`.
  pub fn get(&self, url: &Url) -> Result<(File, HeadersMap), ErrBox> {
    let cache_filename = self.location.join(url_to_filename(url));
    let metadata_filename = Metadata::filename(&cache_filename);
    let file = File::open(cache_filename)?;
    let metadata = fs::read_to_string(metadata_filename)?;
    let metadata: Metadata = serde_json::from_str(&metadata)?;
    Ok((file, metadata.headers))
  }

  pub fn get_metadata(&self, url: &Url) -> Result<Metadata, ErrBox> {
    let cache_filename = self.location.join(url_to_filename(url));
    let metadata_filename = Metadata::filename(&cache_filename);
    let metadata = fs::read_to_string(metadata_filename)?;
    let metadata: Metadata = serde_json::from_str(&metadata)?;
    Ok(metadata)
  }

  pub fn set(
    &self,
    url: &Url,
    headers_map: HeadersMap,
    content: &[u8],
  ) -> Result<(), ErrBox> {
    let cache_filename = self.location.join(url_to_filename(url));
    // Create parent directory
    let parent_filename = cache_filename
      .parent()
      .expect("Cache filename should have a parent dir");
    fs::create_dir_all(parent_filename)?;
    // Cache content
    deno_fs::write_file(&cache_filename, content, 0o666)?;

    let metadata = Metadata {
      url: url.to_string(),
      headers: headers_map,
    };
    metadata.write(&cache_filename)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashMap;
  use std::io::Read;
  use tempfile::TempDir;

  #[test]
  fn test_create_cache() {
    let dir = TempDir::new().unwrap();
    let mut cache_path = dir.path().to_owned();
    cache_path.push("foobar");
    let cache = HttpCache::new(&cache_path);
    assert!(cache.ensure_location().is_ok());
    assert!(cache_path.is_dir());
  }

  #[test]
  fn test_get_set() {
    let dir = TempDir::new().unwrap();
    let cache = HttpCache::new(dir.path());
    let url = Url::parse("https://deno.land/x/welcome.ts").unwrap();
    let mut headers = HashMap::new();
    headers.insert(
      "content-type".to_string(),
      "application/javascript".to_string(),
    );
    headers.insert("etag".to_string(), "as5625rqdsfb".to_string());
    let content = b"Hello world";
    let r = cache.set(&url, headers, content);
    eprintln!("result {:?}", r);
    assert!(r.is_ok());
    let r = cache.get(&url);
    assert!(r.is_ok());
    let (mut file, headers) = r.unwrap();
    let mut content = String::new();
    file.read_to_string(&mut content).unwrap();
    assert_eq!(content, "Hello world");
    assert_eq!(
      headers.get("content-type").unwrap(),
      "application/javascript"
    );
    assert_eq!(headers.get("etag").unwrap(), "as5625rqdsfb");
    assert_eq!(headers.get("foobar"), None);
    drop(dir);
  }

  #[test]
  fn test_url_to_filename() {
    let test_cases = [
      ("https://deno.land/x/foo.ts", "https/deno.land/2c0a064891b9e3fbe386f5d4a833bce5076543f5404613656042107213a7bbc8"),
      (
        "https://deno.land:8080/x/foo.ts",
        "https/deno.land_PORT8080/2c0a064891b9e3fbe386f5d4a833bce5076543f5404613656042107213a7bbc8",
      ),
      ("https://deno.land/", "https/deno.land/8a5edab282632443219e051e4ade2d1d5bbc671c781051bf1437897cbdfea0f1"),
      (
        "https://deno.land/?asdf=qwer",
        "https/deno.land/e4edd1f433165141015db6a823094e6bd8f24dd16fe33f2abd99d34a0a21a3c0",
      ),
      // should be the same as case above, fragment (#qwer) is ignored
      // when hashing
      (
        "https://deno.land/?asdf=qwer#qwer",
        "https/deno.land/e4edd1f433165141015db6a823094e6bd8f24dd16fe33f2abd99d34a0a21a3c0",
      ),
    ];

    for (url, expected) in test_cases.iter() {
      let u = Url::parse(url).unwrap();
      let p = url_to_filename(&u);
      assert_eq!(p, PathBuf::from(expected));
    }
  }
}
