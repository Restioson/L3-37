//! Postgres adapater for l3-37 pool
// #![deny(missing_docs, missing_debug_implementations)]

extern crate futures;
pub extern crate l337;
extern crate redis;
extern crate tokio;
#[macro_use]
extern crate async_trait;
#[macro_use]
extern crate log;

use futures::channel::oneshot;
use redis::aio::{ConnectionLike, MultiplexedConnection};
use redis::{Client, Cmd, IntoConnectionInfo, Pipeline, RedisError, RedisFuture, Value};

type Result<T> = std::result::Result<T, RedisError>;

/// A `ManageConnection` for `RedisConnections`s.
#[derive(Debug)]
pub struct RedisConnectionManager {
    client: redis::Client,
}

impl RedisConnectionManager {
    /// Create a new `RedisConnectionManager`.
    pub fn new(params: impl IntoConnectionInfo) -> Result<RedisConnectionManager> {
        Ok(RedisConnectionManager {
            client: Client::open(params)?,
        })
    }
}

pub struct AsyncConnection {
    conn: MultiplexedConnection,
    receiver: oneshot::Receiver<()>,
    broken: bool,
}

impl ConnectionLike for AsyncConnection {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        self.conn.req_packed_command(cmd)
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        cmd: &'a Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        self.conn.req_packed_commands(cmd, offset, count)
    }

    fn get_db(&self) -> i64 {
        self.conn.get_db()
    }
}

#[async_trait]
impl l337::ManageConnection for RedisConnectionManager {
    type Connection = AsyncConnection;
    type Error = RedisError;

    async fn connect(&self) -> std::result::Result<Self::Connection, l337::Error<Self::Error>> {
        let (connection, future) = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(l337::Error::External)?;

        let (tx, rx) = oneshot::channel();

        tokio::spawn(async move {
            future.await;
            debug!("Future backing redis connection ended, future calls to this redis connection will fail");

            if let Err(e) = tx.send(()) {
                error!(
                    "Failed to alert redis client that connection has ended: {:?}",
                    e
                );
            }
        });

        Ok(AsyncConnection {
            conn: connection,
            broken: false,
            receiver: rx,
        })
    }

    async fn is_valid(
        &self,
        conn: &mut Self::Connection,
    ) -> std::result::Result<(), l337::Error<Self::Error>> {
        redis::cmd("PING")
            .query_async::<_, ()>(conn)
            .await
            .map_err(l337::Error::External)?;

        Ok(())
    }

    fn has_broken(&self, conn: &mut Self::Connection) -> bool {
        if conn.broken {
            return true;
        }

        // Use try_recv() as `has_broken` can be called via Drop and not have a
        // future Context to poll on.
        // https://docs.rs/futures/0.3.1/futures/channel/oneshot/struct.Receiver.html#method.try_recv
        match conn.receiver.try_recv() {
            // If we get any message, the connection task stopped, which means this connection is
            // now dead
            Ok(Some(())) => {
                conn.broken = true;
                true
            }
            // If the future isn't ready, then we haven't sent a value which means the future is
            // still successfully running
            Ok(None) => false,
            // This can happen if the future that the connection was
            // spawned in panicked or was dropped.
            Err(err) => {
                warn!("cannot receive from connection future - err: {}", err);
                conn.broken = true;
                true
            }
        }
    }

    fn timed_out(&self) -> l337::Error<Self::Error> {
        unimplemented!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use l337::{Config, Pool};

    #[tokio::test]
    async fn it_works() {
        let mngr = RedisConnectionManager::new("redis://redis:6379/0").unwrap();

        let config: Config = Default::default();

        let pool = Pool::new(mngr, config).await.unwrap();
        let mut conn = pool.connection().await.unwrap();
        redis::cmd("PING")
            .query_async::<_, ()>(&mut *conn)
            .await
            .unwrap();

        println!("done ping")
    }
}
