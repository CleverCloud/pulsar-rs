use crate::connection::Connection;
use crate::connection_manager::{BrokerAddress, ConnectionManager};
use crate::error::ServiceDiscoveryError;
use crate::executor:: Executor;
use crate::message::proto::{command_lookup_topic_response, CommandLookupTopicResponse};
use futures::{future::try_join_all, FutureExt};
use std::sync::Arc;

/// Look up broker addresses for topics and partitioned topics
///
/// The ServiceDiscovery object provides a single interface to start
/// interacting with a cluster. It will automatically follow redirects
/// or use a proxy, and aggregate broker connections
#[derive(Clone)]
pub struct ServiceDiscovery<Exe: Executor + ?Sized> {
    manager: Arc<ConnectionManager<Exe>>,
}

impl<Exe: Executor> ServiceDiscovery<Exe> {
    pub fn with_manager(
        manager: Arc<ConnectionManager<Exe>>,
    ) -> Self {
        ServiceDiscovery {
            manager,
        }
    }

    /// get the broker address for a topic
    pub async fn lookup_topic<S: Into<String>>(
        &self,
        topic: S,
    ) -> Result<BrokerAddress, ServiceDiscoveryError> {
        let topic = topic.into();
        let conn_info = self.manager.get_connection_from_url(None).await;
        let base_url = self.manager.url.clone();
        let authoritative = false;

        if let Some((proxied_query, conn)) = conn_info {
            self.lookup(
                topic.clone(),
                proxied_query,
                conn.clone(),
                base_url,
                authoritative,
            )
            .await
        } else {
            Err(ServiceDiscoveryError::Query(
                "unknown broker URL".to_string(),
            ))
        }
    }

    /// get the number of partitions for a partitioned topic
    pub async fn lookup_partitioned_topic_number<S: Into<String>>(
        &self,
        topic: S,
    ) -> Result<u32, ServiceDiscoveryError> {
        let connection = self.manager.get_base_connection().await?;

        let response = connection.sender().lookup_partitioned_topic(topic).await?;

        match response.partitions {
            Some(partitions) => Ok(partitions),
            None => {
                if let Some(s) = response.message {
                    Err(ServiceDiscoveryError::Query(s))
                } else {
                    Err(ServiceDiscoveryError::Query(format!(
                        "server error: {:?}",
                        response.error
                    )))
                }
            }
        }
    }

    /// get the list of topic names and addresses for a partitioned topic
    pub async fn lookup_partitioned_topic<S: Into<String>>(
        &self,
        topic: S,
    ) -> Result<Vec<(String, BrokerAddress)>, ServiceDiscoveryError> {
        let topic = topic.into();
        let partitions = self.lookup_partitioned_topic_number(&topic).await?;
        let topics = (0..partitions)
            .map(|nb| {
                let t = format!("{}-partition-{}", topic, nb);
                self.lookup_topic(t.clone())
                    .map(move |address_res| match address_res {
                        Err(e) => Err(e),
                        Ok(address) => Ok((t, address)),
                    })
            })
            .collect::<Vec<_>>();
        try_join_all(topics).await
    }

    pub async fn lookup(
        &self,
        topic: String,
        mut proxied_query: bool,
        mut conn: Arc<Connection>,
        base_url: String,
        mut is_authoritative: bool,
    ) -> Result<BrokerAddress, ServiceDiscoveryError> {
        loop {
            let response = conn
                .sender()
                .lookup_topic(topic.to_string(), is_authoritative)
                .await?;
            let LookupResponse {
                broker_url,
                broker_url_tls,
                proxy,
                redirect,
                authoritative,
            } = convert_lookup_response(&response)?;
            is_authoritative = authoritative;

            // use the TLS connection if available
            let broker_url = broker_url_tls.unwrap_or(broker_url);

            // if going through a proxy, we use the base URL
            let url = if proxied_query || proxy {
                base_url.clone()
            } else {
                broker_url.clone()
            };

            let b = BrokerAddress {
                url,
                broker_url,
                proxy: proxied_query || proxy,
            };

            // if the response indicated a redirect, do another query
            // to the target broker
            let broker_address: BrokerAddress = if redirect {
                let broker_url = Some(b.broker_url);
                let conn_info = self.manager.get_connection_from_url(broker_url).await;
                if let Some((new_proxied_query, new_conn)) = conn_info {
                    proxied_query = new_proxied_query;
                    conn = new_conn.clone();
                    continue;
                } else {
                    return Err(ServiceDiscoveryError::Query(
                        "unknown broker URL".to_string(),
                    ));
                }
            } else {
                b
            };

            let res = self
                .manager
                .get_connection(&broker_address.clone())
                .await
                .map(|_| broker_address)
                .map_err(|e| ServiceDiscoveryError::Connection(e));
            break res;
        }
    }
}

struct LookupResponse {
    pub broker_url: String,
    pub broker_url_tls: Option<String>,
    pub proxy: bool,
    pub redirect: bool,
    pub authoritative: bool,
}

/// extracts information from a lookup response
fn convert_lookup_response(
    response: &CommandLookupTopicResponse,
) -> Result<LookupResponse, ServiceDiscoveryError> {
    if response.response.is_none()
        || response.response == Some(command_lookup_topic_response::LookupType::Failed as i32)
    {
        if let Some(ref s) = response.message {
            return Err(ServiceDiscoveryError::Query(s.to_string()));
        } else {
            return Err(ServiceDiscoveryError::Query(format!(
                "server error: {:?}",
                response.error.unwrap()
            )));
        }
    }

    let proxy = response.proxy_through_service_url.unwrap_or(false);
    let authoritative = response.authoritative.unwrap_or(false);
    let redirect =
        response.response == Some(command_lookup_topic_response::LookupType::Redirect as i32);

    if response.broker_service_url.is_none() {
      return Err(ServiceDiscoveryError::NotFound);
    }

    Ok(LookupResponse {
        broker_url: response.broker_service_url.clone().unwrap(),
        broker_url_tls: response.broker_service_url_tls.clone(),
        proxy,
        redirect,
        authoritative,
    })
}
