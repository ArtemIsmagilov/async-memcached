//! A Tokio-based memcached client.
#![deny(warnings, missing_docs)]
use std::collections::HashMap;

use bytes::BytesMut;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

mod connection;
use self::connection::Connection;

mod error;
pub use self::error::Error;

mod parser;
use self::parser::{
    parse_ascii_metadump_response, parse_ascii_response, parse_ascii_stats_response, Response,
};
pub use self::parser::{ErrorKind, KeyMetadata, MetadumpResponse, StatsResponse, Status, Value};

mod value_serializer;
pub use self::value_serializer::AsMemcachedValue;

/// High-level memcached client.
///
/// [`Client`] is mapped one-to-one with a given connection to a memcached server, and provides a
/// high-level API for executing commands on that connection.
pub struct Client {
    buf: BytesMut,
    last_read_n: Option<usize>,
    conn: Connection,
}

impl Client {
    /// Creates a new [`Client`] based on the given data source string.
    ///
    /// Supports UNIX domain sockets and TCP connections.
    /// For TCP: the DSN should be in the format of `tcp://<IP>:<port>` or `<IP>:<port>`.
    /// For UNIX: the DSN should be in the format of `unix://<path>`.
    pub async fn new<S: AsRef<str>>(dsn: S) -> Result<Client, Error> {
        let connection = Connection::new(dsn).await?;

        Ok(Client {
            buf: BytesMut::new(),
            last_read_n: None,
            conn: connection,
        })
    }

    pub(crate) async fn drive_receive<R, F>(&mut self, op: F) -> Result<R, Error>
    where
        F: Fn(&[u8]) -> Result<Option<(usize, R)>, ErrorKind>,
    {
        // If we serviced a previous request, advance our buffer forward.
        if let Some(n) = self.last_read_n {
            let _ = self.buf.split_to(n);
        }

        let mut needs_more_data = false;
        loop {
            if self.buf.is_empty() || needs_more_data {
                match self.conn {
                    Connection::Tcp(ref mut s) => {
                        self.buf.reserve(1024);
                        let n = s.read_buf(&mut self.buf).await?;
                        if n == 0 {
                            return Err(Error::Io(std::io::ErrorKind::UnexpectedEof.into()));
                        }
                    }
                    Connection::Unix(ref mut s) => {
                        self.buf.reserve(1024);
                        let n = s.read_buf(&mut self.buf).await?;
                        if n == 0 {
                            return Err(Error::Io(std::io::ErrorKind::UnexpectedEof.into()));
                        }
                    }
                }
            }

            // Try and parse out a response.
            match op(&self.buf) {
                // We got a response.
                Ok(Some((n, response))) => {
                    self.last_read_n = Some(n);
                    return Ok(response);
                }
                // We didn't have enough data, so loop around and try again.
                Ok(None) => {
                    needs_more_data = true;
                    continue;
                }
                // Invalid data not matching the protocol.
                Err(kind) => return Err(Status::Error(kind).into()),
            }
        }
    }

    pub(crate) async fn get_read_write_response(&mut self) -> Result<Response, Error> {
        self.drive_receive(parse_ascii_response).await
    }

    pub(crate) async fn filter_set_multi_responses<I, K, V>(
        &mut self,
        kvp: I,
    ) -> Result<HashMap<K, Result<Response, Error>>, Error>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<[u8]> + Eq + std::hash::Hash,
        V: AsMemcachedValue,
    {
        let mut results = HashMap::new();

        for (key, _) in kvp {
            let result = match self.drive_receive(parse_ascii_response).await {
                Ok(Response::Status(Status::Stored)) => Ok(Response::Status(Status::Stored)),
                Ok(Response::Status(s)) => Err(s.into()),
                Ok(_) => Err(Status::Error(ErrorKind::Protocol(None)).into()),
                Err(e) => return Err(e),
            };

            if let Ok(Response::Status(Status::Stored)) = result {
                continue; // skip the insert if the server sends a STORED response
            }

            results.insert(key, result);
        }

        Ok(results)
    }

    pub(crate) async fn get_metadump_response(&mut self) -> Result<MetadumpResponse, Error> {
        self.drive_receive(parse_ascii_metadump_response).await
    }

    pub(crate) async fn get_stats_response(&mut self) -> Result<StatsResponse, Error> {
        self.drive_receive(parse_ascii_stats_response).await
    }

    /// Gets the given key.
    ///
    /// If the key is found, `Some(Value)` is returned, describing the metadata and data of the key.
    ///
    /// Otherwise, [`Error`] is returned.
    pub async fn get<K: AsRef<[u8]>>(&mut self, key: K) -> Result<Option<Value>, Error> {
        self.conn.write_all(b"get ").await?;
        self.conn.write_all(key.as_ref()).await?;
        self.conn.write_all(b"\r\n").await?;
        self.conn.flush().await?;

        match self.get_read_write_response().await? {
            Response::Status(Status::NotFound) => Ok(None),
            Response::Status(s) => Err(s.into()),
            Response::Data(d) => d
                .map(|mut items| {
                    if items.len() != 1 {
                        Err(Status::Error(ErrorKind::Protocol(None)).into())
                    } else {
                        Ok(items.remove(0))
                    }
                })
                .transpose(),
            _ => Err(Error::Protocol(Status::Error(ErrorKind::Protocol(None)))),
        }
    }

    /// Gets the given keys.
    ///
    /// If any of the keys are found, a vector of [`Value`] will be returned, where [`Value`]
    /// describes the metadata and data of the key.
    ///
    /// Otherwise, [`Error`] is returned.
    /// This will eventually be deprecated in favor of `get_multi`
    pub async fn get_many<I, K>(&mut self, keys: I) -> Result<Vec<Value>, Error>
    where
        I: IntoIterator<Item = K>,
        K: AsRef<[u8]>,
    {
        self.conn.write_all(b"get ").await?;
        for key in keys {
            self.conn.write_all(key.as_ref()).await?;
            self.conn.write_all(b" ").await?;
        }
        self.conn.write_all(b"\r\n").await?;
        self.conn.flush().await?;

        match self.get_read_write_response().await? {
            Response::Status(s) => Err(s.into()),
            Response::Data(d) => d.ok_or(Status::NotFound.into()),
            _ => Err(Status::Error(ErrorKind::Protocol(None)).into()),
        }
    }

    /// Sets the given key.
    ///
    /// If `ttl` or `flags` are not specified, they will default to 0.  If the value is set
    /// successfully, `()` is returned, otherwise [`Error`] is returned.
    pub async fn set<K, V>(
        &mut self,
        key: K,
        value: V,
        ttl: Option<i64>,
        flags: Option<u32>,
    ) -> Result<(), Error>
    where
        K: AsRef<[u8]>,
        V: AsMemcachedValue,
    {
        let kr = key.as_ref();
        let vr = value.as_bytes();

        self.conn.write_all(b"set ").await?;
        self.conn.write_all(kr).await?;

        let flags = flags.unwrap_or(0).to_string();
        self.conn.write_all(b" ").await?;
        self.conn.write_all(flags.as_ref()).await?;

        let ttl = ttl.unwrap_or(0).to_string();
        self.conn.write_all(b" ").await?;
        self.conn.write_all(ttl.as_ref()).await?;

        let vlen = vr.len().to_string();
        self.conn.write_all(b" ").await?;
        self.conn.write_all(vlen.as_ref()).await?;
        self.conn.write_all(b"\r\n").await?;

        self.conn.write_all(vr.as_ref()).await?;
        self.conn.write_all(b"\r\n").await?;

        self.conn.flush().await?;

        match self.get_read_write_response().await? {
            Response::Status(Status::Stored) => Ok(()),
            Response::Status(s) => Err(s.into()),
            _ => Err(Status::Error(ErrorKind::Protocol(None)).into()),
        }
    }

    /// Sets the given key without waiting for a reply from the server.
    ///
    /// If `ttl` or `flags` are not specified, they will default to 0.
    pub async fn set_no_reply<K, V>(
        &mut self,
        key: K,
        value: V,
        ttl: Option<i64>,
        flags: Option<u32>,
    ) -> Result<(), Error>
    where
        K: AsRef<[u8]>,
        V: AsMemcachedValue,
    {
        let kr = key.as_ref();
        let vr = value.as_bytes();

        self.conn.write_all(b"set ").await?;
        self.conn.write_all(kr).await?;

        let flags = flags.unwrap_or(0).to_string();
        self.conn.write_all(b" ").await?;
        self.conn.write_all(flags.as_ref()).await?;

        let ttl = ttl.unwrap_or(0).to_string();
        self.conn.write_all(b" ").await?;
        self.conn.write_all(ttl.as_ref()).await?;

        let vlen = vr.len().to_string();
        self.conn.write_all(b" ").await?;
        self.conn.write_all(vlen.as_ref()).await?;
        self.conn.write_all(b" noreply\r\n").await?;

        self.conn.write_all(vr.as_ref()).await?;
        self.conn.write_all(b"\r\n").await?;

        self.conn.flush().await?;

        Ok(())
    }

    /// Sets multiple keys.
    ///
    /// If `ttl` or `flags` are not specified, they will default to 0.  If the value is set
    /// successfully, `()` is returned, otherwise [`Error`] is returned.
    pub async fn set_multi<I, K, V>(
        &mut self,
        kv: I,
        ttl: Option<i64>,
        flags: Option<u32>,
    ) -> Result<HashMap<K, Result<Response, Error>>, Error>
    where
        I: IntoIterator<Item = (K, V)> + Clone,
        K: AsRef<[u8]> + Eq + std::hash::Hash + std::fmt::Debug,
        V: AsMemcachedValue,
    {
        let mut kv_iter = kv.clone().into_iter().peekable();

        if kv_iter.peek().is_none() {
            return Ok(HashMap::new());
        }

        for (key, value) in kv {
            let kr = key.as_ref();
            let vr = value.as_bytes();

            self.conn.write_all(b"set ").await?;
            self.conn.write_all(kr).await?;

            let flags = flags.unwrap_or(0).to_string();
            self.conn.write_all(b" ").await?;
            self.conn.write_all(flags.as_ref()).await?;

            let ttl = ttl.unwrap_or(0).to_string();
            self.conn.write_all(b" ").await?;
            self.conn.write_all(ttl.as_ref()).await?;

            let vlen = vr.len().to_string();
            self.conn.write_all(b" ").await?;
            self.conn.write_all(vlen.as_ref()).await?;
            self.conn.write_all(b"\r\n").await?;

            self.conn.write_all(vr.as_ref()).await?;
            self.conn.write_all(b"\r\n").await?;
        }
        self.conn.flush().await?;

        let results = self.filter_set_multi_responses(kv_iter).await?;

        Ok(results)
    }

    /// Sets multiple keys.
    ///
    /// If `ttl` or `flags` are not specified, they will default to 0.  If the value is set
    /// successfully, `()` is returned, otherwise [`Error`] is returned.
    pub async fn set_multi_test_one<I, K, V>(
        &mut self,
        kv: I,
        ttl: Option<i64>,
        flags: Option<u32>,
    ) -> Result<HashMap<K, Result<Response, Error>>, Error>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<[u8]> + Eq + std::hash::Hash + std::fmt::Debug,
        V: AsMemcachedValue,
    {
        let mut results = HashMap::new();
        let mut kv_iter = kv.into_iter().peekable();

        if kv_iter.peek().is_none() {
            return Ok(results);
        }

        for (key, value) in kv_iter {
            self.write_set_command(&key, &value, ttl, flags).await?;
            self.conn.flush().await?;
            let response = match self.get_read_write_response().await {
                Ok(Response::Status(Status::Stored)) => Ok(Response::Status(Status::Stored)),
                Ok(Response::Status(s)) => Err(s.into()),
                Ok(_) => Err(Status::Error(ErrorKind::Protocol(None)).into()),
                Err(e) => return Err(e),
            };

            if let Ok(Response::Status(Status::Stored)) = response {
                continue;
            }

            results.insert(key, response);
        }

        Ok(results)
    }

    // Used by set_multi_test_one
    async fn write_set_command<K: AsRef<[u8]>, V: AsMemcachedValue>(
        &mut self,
        key: &K,
        value: &V,
        ttl: Option<i64>,
        flags: Option<u32>,
    ) -> Result<(), Error> {
        let kr = key.as_ref();
        let vr = value.as_bytes();

        self.conn.write_all(b"set ").await?;
        self.conn.write_all(kr).await?;

        let flags = flags.unwrap_or(0).to_string();
        self.conn.write_all(b" ").await?;
        self.conn.write_all(flags.as_ref()).await?;

        let ttl = ttl.unwrap_or(0).to_string();
        self.conn.write_all(b" ").await?;
        self.conn.write_all(ttl.as_ref()).await?;

        let vlen = vr.len().to_string();
        self.conn.write_all(b" ").await?;
        self.conn.write_all(vlen.as_ref()).await?;
        self.conn.write_all(b"\r\n").await?;

        self.conn.write_all(vr.as_ref()).await?;
        self.conn.write_all(b"\r\n").await?;

        Ok(())
    }

    /// Sets multiple keys.
    ///
    /// If `ttl` or `flags` are not specified, they will default to 0.  If the value is set
    /// successfully, `()` is returned, otherwise [`Error`] is returned.
    pub async fn set_multi_test_two<I, K, V>(
        &mut self,
        kv: I,
        ttl: Option<i64>,
        flags: Option<u32>,
    ) -> Result<HashMap<K, Result<Response, Error>>, Error>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<[u8]> + Eq + std::hash::Hash,
        V: AsMemcachedValue,
    {
        // This method avoids copying the whole kv and instead copies keys to a new vec in the order that they're processed.
        let mut keys = Vec::new();

        for (key, value) in kv {
            let kr = key.as_ref();
            let vr = value.as_bytes();

            self.conn.write_all(b"set ").await?;
            self.conn.write_all(kr).await?;

            let flags = flags.unwrap_or(0).to_string();
            self.conn.write_all(b" ").await?;
            self.conn.write_all(flags.as_ref()).await?;

            let ttl = ttl.unwrap_or(0).to_string();
            self.conn.write_all(b" ").await?;
            self.conn.write_all(ttl.as_ref()).await?;

            let vlen = vr.len().to_string();
            self.conn.write_all(b" ").await?;
            self.conn.write_all(vlen.as_ref()).await?;
            self.conn.write_all(b"\r\n").await?;

            self.conn.write_all(vr.as_ref()).await?;
            self.conn.write_all(b"\r\n").await?;

            keys.push(key);
        }
        self.conn.flush().await?;

        // With this approach we can also allocate the proper size hashmap up front.
        let mut results: HashMap<K, Result<Response, Error>> = HashMap::with_capacity(keys.len());

        // Inline the previous filter_set_multi_responses behaviour.
        for key in keys {
            let result = match self.drive_receive(parse_ascii_response).await {
                Ok(Response::Status(Status::Stored)) => Ok(Response::Status(Status::Stored)),
                Ok(Response::Status(s)) => Err(s.into()),
                Ok(_) => Err(Status::Error(ErrorKind::Protocol(None)).into()),
                Err(e) => return Err(e),
            };
            if let Ok(Response::Status(Status::Stored)) = result {
                continue;
            }
            results.insert(key, result);
        }

        Ok(results)
    }

    /// Sets the given keys.
    ///
    /// If `ttl` or `flags` are not specified, they will default to 0.  If the value is set
    /// successfully, `()` is returned, otherwise [`Error`] is returned.
    pub async fn set_multi_loop<I, K, V>(
        &mut self,
        kv: I,
        ttl: Option<i64>,
        flags: Option<u32>,
    ) -> Result<HashMap<K, Result<(), Error>>, Error>
    where
        I: IntoIterator<Item = (K, V)> + Clone,
        K: AsRef<[u8]> + Eq + std::hash::Hash,
        V: AsMemcachedValue,
    {
        let mut kv_iter = kv.into_iter().peekable();

        if kv_iter.peek().is_none() {
            return Ok(HashMap::new());
        }

        let mut error_map: HashMap<K, Result<(), Error>> = HashMap::new();

        // Write commands and collect key-error pairs
        for (key, value) in kv_iter {
            let response = self.set(&key, value, ttl, flags).await;

            if response.is_err() {
                error_map.insert(key, response);
            }
        }

        Ok(error_map)
    }

    /// Add a key. If the value exists, Err(Protocol(NotStored)) is returned.
    pub async fn add<K, V>(
        &mut self,
        key: K,
        value: V,
        ttl: Option<i64>,
        flags: Option<u32>,
    ) -> Result<(), Error>
    where
        K: AsRef<[u8]>,
        V: AsMemcachedValue,
    {
        let kr = key.as_ref();
        let vr = value.as_bytes();

        self.conn.write_all(b"add ").await?;
        self.conn.write_all(kr).await?;

        let flags = flags.unwrap_or(0).to_string();
        self.conn.write_all(b" ").await?;
        self.conn.write_all(flags.as_ref()).await?;

        let ttl = ttl.unwrap_or(0).to_string();
        self.conn.write_all(b" ").await?;
        self.conn.write_all(ttl.as_ref()).await?;

        let vlen = vr.len().to_string();
        self.conn.write_all(b" ").await?;
        self.conn.write_all(vlen.as_ref()).await?;
        self.conn.write_all(b"\r\n").await?;

        self.conn.write_all(vr.as_ref()).await?;
        self.conn.write_all(b"\r\n").await?;

        self.conn.flush().await?;

        match self.get_read_write_response().await? {
            Response::Status(Status::Stored) => Ok(()),
            Response::Status(s) => Err(s.into()),
            _ => Err(Status::Error(ErrorKind::Protocol(None)).into()),
        }
    }

    /// Delete a key but don't wait for a reply.
    pub async fn delete_no_reply<K>(&mut self, key: K) -> Result<(), Error>
    where
        K: AsRef<[u8]>,
    {
        let kr = key.as_ref();

        self.conn
            .write_all(&[b"delete ", kr, b" noreply\r\n"].concat())
            .await?;
        self.conn.flush().await?;
        Ok(())
    }

    /// Delete a key and wait for a reply
    pub async fn delete<K>(&mut self, key: K) -> Result<(), Error>
    where
        K: AsRef<[u8]>,
    {
        let kr = key.as_ref();

        self.conn
            .write_all(&[b"delete ", kr, b"\r\n"].concat())
            .await?;
        self.conn.flush().await?;

        match self.get_read_write_response().await? {
            Response::Status(Status::Deleted) => Ok(()),
            Response::Status(s) => Err(s.into()),
            _ => Err(Status::Error(ErrorKind::Protocol(None)).into()),
        }
    }

    /// Increments the given key by the specified amount.
    /// Can overflow from the max value of u64 (18446744073709551615) -> 0.
    /// If the key does not exist, the server will return a KeyNotFound error.
    /// If the key exists but the value is non-numeric, the server will return a ClientError.
    pub async fn increment<K>(&mut self, key: K, amount: u64) -> Result<u64, Error>
    where
        K: AsRef<[u8]>,
    {
        self.conn
            .write_all(
                &[
                    b"incr ",
                    key.as_ref(),
                    b" ",
                    amount.to_string().as_bytes(),
                    b"\r\n",
                ]
                .concat(),
            )
            .await?;
        self.conn.flush().await?;

        match self.get_read_write_response().await? {
            Response::Status(s) => Err(s.into()),
            Response::IncrDecr(amount) => Ok(amount),
            _ => Err(Status::Error(ErrorKind::Protocol(None)).into()),
        }
    }

    /// Increments the given key by the specified amount with no reply from the server.
    /// Can overflow from the max value of u64 (18446744073709551615) -> 0.
    /// Always returns Ok(()), will not return any indication of success or failure.
    pub async fn increment_no_reply<K>(&mut self, key: K, amount: u64) -> Result<(), Error>
    where
        K: AsRef<[u8]>,
    {
        self.conn
            .write_all(
                &[
                    b"incr ",
                    key.as_ref(),
                    b" ",
                    amount.to_string().as_bytes(),
                    b" noreply\r\n",
                ]
                .concat(),
            )
            .await?;
        self.conn.flush().await?;

        Ok(())
    }

    /// Decrements the given key by the specified amount.
    /// Will not decrement the counter below 0.
    /// If the key does not exist, the server will return a KeyNotFound error.
    /// If the key exists but the value is non-numeric, the server will return a ClientError.
    pub async fn decrement<K>(&mut self, key: K, amount: u64) -> Result<u64, Error>
    where
        K: AsRef<[u8]>,
    {
        self.conn
            .write_all(
                &[
                    b"decr ",
                    key.as_ref(),
                    b" ",
                    amount.to_string().as_bytes(),
                    b"\r\n",
                ]
                .concat(),
            )
            .await?;
        self.conn.flush().await?;

        match self.get_read_write_response().await? {
            Response::Status(s) => Err(s.into()),
            Response::IncrDecr(amount) => Ok(amount),
            _ => Err(Status::Error(ErrorKind::Protocol(None)).into()),
        }
    }

    /// Decrements the given key by the specified amount with no reply from the server.
    /// Will not decrement the counter below 0.
    /// Returns the new value of the key if key exists, otherwise returns KeyNotFound error.
    pub async fn decrement_no_reply<K>(&mut self, key: K, amount: u64) -> Result<(), Error>
    where
        K: AsRef<[u8]>,
    {
        self.conn
            .write_all(
                &[
                    b"decr ",
                    key.as_ref(),
                    b" ",
                    amount.to_string().as_bytes(),
                    b" noreply\r\n",
                ]
                .concat(),
            )
            .await?;
        self.conn.flush().await?;

        Ok(())
    }

    /// Gets the version of the server.
    ///
    /// If the version is retrieved successfully, `String` is returned containing the version
    /// component e.g. `1.6.7`, otherwise [`Error`] is returned.
    ///
    /// For some setups, such as those using Twemproxy, this will return an error as those
    /// intermediate proxies do not support the version command.
    pub async fn version(&mut self) -> Result<String, Error> {
        self.conn.write_all(b"version\r\n").await?;
        self.conn.flush().await?;

        let mut version = String::new();
        let bytes = self.conn.read_line(&mut version).await?;

        // Peel off the leading "VERSION " header.
        if bytes >= 8 && version.is_char_boundary(8) {
            Ok(version.split_off(8))
        } else {
            Err(Error::from(Status::Error(ErrorKind::Protocol(Some(
                format!("Invalid response for `version` command: `{version}`"),
            )))))
        }
    }

    /// Dumps all keys from the server.
    ///
    /// This operation scans all slab classes from tail to head, in a non-blocking fashion.  Thus,
    /// not all items will be found as new items could be inserted or deleted while the crawler is
    /// still running.
    ///
    /// [`MetadumpIter`] must be iterated over to discover whether or not the crawler successfully
    /// started, as this call will only return [`Error`] if the command failed to be written to the
    /// server at all.
    ///
    /// Available as of memcached 1.4.31.
    pub async fn dump_keys(&mut self) -> Result<MetadumpIter<'_>, Error> {
        self.conn.write_all(b"lru_crawler metadump all\r\n").await?;
        self.conn.flush().await?;

        Ok(MetadumpIter {
            client: self,
            done: false,
        })
    }

    /// Collects statistics from the server.
    ///
    /// The statistics that may be returned are detailed in the protocol specification for
    /// memcached, but all values returned by this method are returned as strings and are not
    /// further interpreted or validated for conformity.
    pub async fn stats(&mut self) -> Result<HashMap<String, String>, Error> {
        let mut entries = HashMap::new();

        self.conn.write_all(b"stats\r\n").await?;
        self.conn.flush().await?;

        while let StatsResponse::Entry(key, value) = self.get_stats_response().await? {
            entries.insert(key, value);
        }

        Ok(entries)
    }
}

/// Asynchronous iterator for metadump operations.
pub struct MetadumpIter<'a> {
    client: &'a mut Client,
    done: bool,
}

impl<'a> MetadumpIter<'a> {
    /// Gets the next result for the current operation.
    ///
    /// If there is another key in the dump, `Some(Ok(KeyMetadata))` will be returned.  If there was
    /// an error while attempting to start the metadump operation, or if there was a general
    /// network/protocol-level error, `Some(Err(Error))` will be returned.
    ///
    /// Otherwise, `None` will be returned and signals the end of the iterator.  Subsequent calls
    /// will return `None`.
    pub async fn next(&mut self) -> Option<Result<KeyMetadata, Error>> {
        if self.done {
            return None;
        }

        match self.client.get_metadump_response().await {
            Ok(MetadumpResponse::End) => {
                self.done = true;
                None
            }
            Ok(MetadumpResponse::BadClass(s)) => {
                self.done = true;
                Some(Err(Error::Protocol(MetadumpResponse::BadClass(s).into())))
            }
            Ok(MetadumpResponse::Busy(s)) => {
                Some(Err(Error::Protocol(MetadumpResponse::Busy(s).into())))
            }
            Ok(MetadumpResponse::Entry(km)) => Some(Ok(km)),
            Err(e) => Some(Err(e)),
        }
    }
}
