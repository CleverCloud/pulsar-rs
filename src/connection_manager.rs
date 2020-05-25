use crate::connection::{Authentication, Connection};
use crate::error::ConnectionError;
use crate::executor::Executor;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::marker::PhantomData;

/// holds connection information for a broker
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct BrokerAddress {
    /// URL we're using for connection (can be the proxy's URL)
    pub url: String,
    /// pulsar URL for the broker
    pub broker_url: String,
    /// true if we're connecting through a proxy
    pub proxy: bool,
}

/// Look up broker addresses for topics and partitioned topics
///
/// The ConnectionManager object provides a single interface to start
/// interacting with a cluster. It will automatically follow redirects
/// or use a proxy, and aggregate broker connections
#[derive(Clone)]
pub struct ConnectionManager<Exe: Executor + ?Sized> {
    pub url: String,
    base: Arc<Connection>,
    auth: Option<Authentication>,
    executor: PhantomData<Exe>,
    connections: Arc<Mutex<HashMap<BrokerAddress, Arc<Connection>>>>,
}

impl<Exe: Executor> ConnectionManager<Exe> {
    pub async fn new(
        url: String,
        auth: Option<Authentication>,
    ) -> Result<Self, ConnectionError> {
        let conn = Connection::new::<Exe>(url.clone(), auth.clone(), None).await?;
        ConnectionManager::from_connection(conn, auth, url)
    }

    pub fn from_connection(
        connection: Connection,
        auth: Option<Authentication>,
        url: String,
    ) -> Result<Self, ConnectionError> {
        let base = Arc::new(connection);
        Ok(ConnectionManager {
            url,
            base,
            auth,
            executor: PhantomData,
            connections: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// get an active Connection from a broker address
    ///
    /// creates a connection if not available
    pub async fn get_base_connection(&self) -> Result<Arc<Connection>, ConnectionError> {
        Ok(self.base.clone())
    }

    /// get an active Connection from a broker address
    ///
    /// creates a connection if not available
    pub async fn get_connection(
        &self,
        broker: &BrokerAddress,
    ) -> Result<Arc<Connection>, ConnectionError> {
        if let Some(conn) = self.connections.lock().unwrap().get(&broker) {
            return Ok(conn.clone());
        }

        self.connect(broker.clone()).await
    }

    pub async fn get_connection_from_url(
        &self,
        broker: Option<String>,
    ) -> Option<(bool, Arc<Connection>)> {
        let res = match broker {
            None => {
                debug!("using the base connection for lookup, not through a proxy");
                Some((false, self.base.clone()))
            }
            Some(ref s) => {
                if let Some((b, c)) = self
                    .connections
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(k, _)| &k.broker_url == s)
                {
                    debug!(
                        "using another connection for lookup, proxying to {:?}",
                        b.proxy
                    );
                    Some((b.proxy, c.clone()))
                } else {
                    None
                }
            }
        };
        res
    }

    async fn connect(&self, broker: BrokerAddress) -> Result<Arc<Connection>, ConnectionError> {
        let proxy_url = if broker.proxy {
            Some(broker.broker_url.clone())
        } else {
            None
        };

        let conn = Connection::new::<Exe>(
            broker.url.clone(),
            self.auth.clone(),
            proxy_url,
        )
        .await?;
        let c = Arc::new(conn);
        self.connections.lock().unwrap().insert(broker, c.clone());
        Ok(c)
    }
}
