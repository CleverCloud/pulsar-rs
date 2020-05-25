use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Debug;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::pin::Pin;

use chrono::{DateTime, Utc};
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::{channel::{mpsc, oneshot}, Future, FutureExt, Stream, StreamExt};
use futures::future::{try_join_all};
use futures::task::{Context, Poll};
use rand;
use regex::Regex;

use crate::connection:: Connection;
use crate::error::{ConnectionError, ConsumerError, Error};
use crate::executor::{Executor, Interval};
use crate::message::{
    parse_batched_message,
    proto::{self, command_subscribe::SubType, MessageIdData, Schema},
    BatchedMessage, Message as RawMessage, Metadata, Payload,
};
use crate::{DeserializeMessage, Pulsar};
use bit_vec::BitVec;
use nom::lib::std::cmp::Ordering;
use nom::lib::std::collections::BinaryHeap;

#[derive(Clone, Default)]
pub struct ConsumerOptions {
    pub priority_level: Option<i32>,
    pub durable: Option<bool>,
    pub start_message_id: Option<MessageIdData>,
    pub metadata: BTreeMap<String, String>,
    pub read_compacted: Option<bool>,
    pub schema: Option<Schema>,
    pub initial_position: Option<i32>,
}

pub struct Consumer<T: DeserializeMessage> {
    connection: Arc<Connection>,
    topic: String,
    id: u64,
    messages: Pin<Box<mpsc::UnboundedReceiver<RawMessage>>>,
    nack_handler: UnboundedSender<NackMessage>,
    batch_size: u32,
    remaining_messages: u32,
    #[allow(unused)]
    data_type: PhantomData<fn(Payload) -> T::Output>,
    options: ConsumerOptions,
    current_message: Option<BatchedMessageIterator>,
    _drop_signal: oneshot::Sender<()>,
}

impl<T: DeserializeMessage> Consumer<T> {
    pub async fn from_connection<Exe: Executor>(
        connection: Arc<Connection>,
        topic: String,
        subscription: String,
        sub_type: SubType,
        consumer_id: Option<u64>,
        consumer_name: Option<String>,
        batch_size: Option<u32>,
        unacked_message_redelivery_delay: Option<Duration>,
        options: ConsumerOptions,
    ) -> Result<Consumer<T>, Error> {
        let consumer_id = consumer_id.unwrap_or_else(rand::random);
        let (resolver, messages) = mpsc::unbounded();
        let batch_size = batch_size.unwrap_or(1000);

        connection.sender()
            .subscribe(
                resolver,
                topic.clone(),
                subscription,
                sub_type,
                consumer_id,
                consumer_name.clone(),
                options.clone(),
            ).await.map_err(Error::Connection)?;

        connection.sender()
            .send_flow(consumer_id, batch_size)
            .map_err(|e| Error::Consumer(ConsumerError::Connection(e)))?;

        //TODO this should be shared among all consumers when using the client
        //TODO make tick_delay configurable
        let tick_delay = Duration::from_millis(500);
        let nack_handler = NackHandler::new::<Exe>(
            connection.clone(),
            unacked_message_redelivery_delay,
            tick_delay,
            );

        // drop_signal will be dropped when Consumer is dropped, then
        // drop_receiver will return, and we can close the consumer
        let (_drop_signal, drop_receiver) = oneshot::channel::<()>();
        let conn = connection.clone();
        let ack_sender = nack_handler.clone();
        let _ = Exe::spawn(Box::pin(async move {
            let _res = drop_receiver.await;
            ack_sender.close_channel();
            if let Err(e) = conn.sender().close_consumer(consumer_id).await {
              error!("could not close consumer {:?}({}): {:?}", consumer_name, consumer_id, e);
            }
        }));

        Ok(Consumer {
            connection,
            topic,
            id: consumer_id,
            messages: Box::pin(messages),
            nack_handler,
            batch_size,
            remaining_messages: batch_size,
            data_type: PhantomData,
            options,
            current_message: None,
            _drop_signal,
        })
    }

    pub fn topic(&self) -> &str {
        &self.topic
    }

    pub fn options(&self) -> &ConsumerOptions {
        &self.options
    }

    pub async fn check_connection(&self) -> Result<(), Error> {
        self.connection
            .sender()
            .send_ping().await?;
        Ok(())
    }

    pub fn ack(&self, msg: &Message<T>) -> Result<(), ConnectionError> {
        self
            .connection
            .sender()
            .send_ack(
                self.id,
                vec![msg.message_id.id.clone()],
                false)
    }

    pub fn cumulative_ack(&self, msg: &Message<T>) -> Result<(), ConnectionError> {
        self
            .connection
            .sender()
            .send_ack(
                self.id,
                vec![msg.message_id.id.clone()],
                true)
    }

    pub fn nack(&self, msg: &Message<T>) {
        let _ = self.nack_handler.unbounded_send(NackMessage {
            consumer_id: self.id,
            message_ids: vec![msg.message_id.clone()],
        });
    }

    fn create_message(
        &self,
        message_id: proto::MessageIdData,
        payload: Payload,
    ) -> Message<T> {
        Message {
            topic: self.topic.clone(),
            message_id: MessageData {
                id: message_id,
                batch_size: payload.metadata.num_messages_in_batch.clone(),
            },
            payload,
            _phantom: PhantomData,
        }
    }

    fn poll_current_message(&mut self) -> Poll<Message<T>> {
        if let Some(mut iterator) = self.current_message.take() {
            match iterator.next() {
                Some((id, payload)) => {
                    self.current_message = Some(iterator);
                    let message = self.create_message(id, payload);
                    Poll::Ready(message)
                }
                None => Poll::Pending,
            }
        } else {
            Poll::Pending
        }
    }
}

#[derive(Clone)]
struct MessageData {
    id: proto::MessageIdData,
    batch_size: Option<i32>,
}

struct NackMessage {
    consumer_id: u64,
    message_ids: Vec<MessageData>,
}

#[derive(Debug, PartialEq, Eq)]
struct MessageResend {
    when: Instant,
    consumer_id: u64,
    message_ids: Vec<MessageIdData>,
}

impl PartialOrd for MessageResend {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// Ordering is defined for use in a BinaryHeap (a max heap), so ordering is reversed to cause
// earlier `when`s to be at the front of the queue
impl Ord for MessageResend {
    fn cmp(&self, other: &Self) -> Ordering {
        self.when.cmp(&other.when).reverse()
    }
}

struct NackHandler {
    pending_nacks: BinaryHeap<MessageResend>,
    conn: Arc<Connection>,
    inbound: Option<Pin<Box<UnboundedReceiver<NackMessage>>>>,
    unack_redelivery_delay: Option<Duration>,
    tick_timer: Pin<Box<Interval>>,
    batch_messages: BTreeMap<MessageIdData, (bool, BitVec)>,
}

impl NackHandler {
    /// Create and spawn a new NackHandler future, which will run until the connection fails, or all
    /// inbound senders are dropped and any pending redelivery messages have been sent
    pub fn new<Exe: Executor + ?Sized>(
        conn: Arc<Connection>,
        redelivery_delay: Option<Duration>,
        tick_delay: Duration,
    ) -> UnboundedSender<NackMessage> {
        let (tx, rx) = mpsc::unbounded();

        if let Err(_) = Exe::spawn(Box::pin(NackHandler {
            pending_nacks: BinaryHeap::new(),
            conn,
            inbound: Some(Box::pin(rx)),
            unack_redelivery_delay: redelivery_delay,
            tick_timer: Box::pin(Exe::interval(tick_delay)),
            batch_messages: BTreeMap::new(),
        }.map(|res| trace!("AckHandler returned {:?}", res)))) {
            error!("the executor could not spawn the AckHandler future");
        }
        tx
    }
    fn next_ready_resend(&mut self) -> Option<MessageResend> {
        if let Some(resend) = self.pending_nacks.peek() {
            if resend.when <= Instant::now() {
                return self.pending_nacks.pop();
            }
        }
        None
    }
    fn next_inbound(&mut self, cx: &mut Context<'_>) -> Option<NackMessage> {
        if let Some(inbound) = &mut self.inbound {
            match inbound.as_mut().poll_next(cx) {
                Poll::Ready(Some(msg)) => Some(msg),
                Poll::Pending => None,
                Poll::Ready(None) => {
                    self.inbound = None;
                    None
                }
            }
        } else {
            None
        }
    }

    fn should_nack(&mut self, message_data: MessageData) -> Option<proto::MessageIdData> {
        let MessageData { mut id, batch_size } = message_data;
        let batch_index = id.batch_index.take();

        match (batch_index, batch_size) {
            (Some(index), Some(size)) if index >= 0 && size > 1 => {
                let (is_nacked, seen_messages) = self
                    .batch_messages
                    .entry(id.clone())
                    .or_insert_with(|| (false, BitVec::from_elem(size as usize, false)));

                seen_messages.set(index as usize, true);
                let should_nack = !*is_nacked;
                *is_nacked = true;
                if seen_messages.all() {
                    self.batch_messages.remove(&id);
                }

                if should_nack {
                    Some(id)
                } else {
                    None
                }
            }
            _ => Some(id),
        }
    }
}

impl Future for NackHandler {
    type Output = Result<(), ()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        while let Some(NackMessage { consumer_id, message_ids }) = self.next_inbound(cx) {
            // if timeout is not set, messages will only be redelivered on reconnect,
            // so we don't manually send redelivery request
            let ids: Vec<proto::MessageIdData> = message_ids
                .into_iter()
                .filter_map(|message| self.should_nack(message))
                .collect();
            if let Some(nack_timeout) = self.unack_redelivery_delay {
                if !ids.is_empty() {
                    self.pending_nacks.push(MessageResend {
                        consumer_id,
                        when: Instant::now() + nack_timeout,
                        message_ids: ids,
                    });
                }
            }
        }

        loop {
            match self.tick_timer.as_mut().poll_next(cx) {
                Poll::Ready(Some(_)) => {
                    let mut resends: BTreeMap<u64, Vec<MessageIdData>> = BTreeMap::new();
                    while let Some(ready) = self.next_ready_resend() {
                        resends
                            .entry(ready.consumer_id)
                            .or_insert_with(Vec::new)
                            .extend(ready.message_ids);
                    }
                    for (consumer_id, message_ids) in resends {
                        //TODO this should be resilient to reconnects
                        let send_result = self
                            .conn
                            .sender()
                            .send_redeliver_unacknowleged_messages(consumer_id, message_ids);
                        if send_result.is_err() {
                            return Poll::Ready(Err(()));
                        }
                    }
                }
                Poll::Pending => {
                    if self.inbound.is_none() && self.pending_nacks.is_empty() {
                        return Poll::Ready(Ok(()));
                    } else {
                        return Poll::Pending;
                    }
                }
                Poll::Ready(None) => return Poll::Ready(Err(())),
            }
        }
    }
}

struct BatchedMessageIterator {
    messages: std::vec::IntoIter<BatchedMessage>,
    message_id: proto::MessageIdData,
    metadata: Metadata,
    total_messages: u32,
    current_index: u32,
}

impl BatchedMessageIterator {
    fn new(message_id: proto::MessageIdData, payload: Payload) -> Result<Self, ConnectionError> {
        let total_messages = payload
            .metadata
            .num_messages_in_batch
            .expect("expected batched message") as u32;
        let messages = parse_batched_message(total_messages, &payload.data)?;

        Ok(Self {
            messages: messages.into_iter(),
            message_id,
            total_messages,
            metadata: payload.metadata,
            current_index: 0,
        })
    }
}

impl Iterator for BatchedMessageIterator {
    type Item = (proto::MessageIdData, Payload);

    fn next(&mut self) -> Option<Self::Item> {
        let remaining = self.total_messages - self.current_index;
        if remaining == 0 {
            return None;
        }
        let index = self.current_index;
        self.current_index += 1;
        if let Some(batched_message) = self.messages.next() {
            let id = proto::MessageIdData {
                batch_index: Some(index as i32),
                ..self.message_id.clone()
            };

            let metadata = Metadata {
                properties: batched_message.metadata.properties,
                partition_key: batched_message.metadata.partition_key,
                event_time: batched_message.metadata.event_time,
                ..self.metadata.clone()
            };

            let payload = Payload {
                metadata,
                data: batched_message.payload,
            };

            Some((id, payload))
        } else {
            None
        }
    }
}

impl<T: DeserializeMessage> Stream for Consumer<T> {
    type Item = Result<Message<T>, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Poll::Ready(message) = self.poll_current_message() {
            return Poll::Ready(Some(Ok(message)));
        }

        if !self.connection.is_valid() {
            if let Some(err) = self.connection.error() {
                return Poll::Ready(Some(Err(Error::Consumer(ConsumerError::Connection(err)))));
            }
        }

        if self.remaining_messages < self.batch_size / 2 {
            self.connection
                .sender()
                .send_flow(self.id, self.batch_size - self.remaining_messages)?;
            self.remaining_messages = self.batch_size;
        }

        let message: Option<Option<(proto::CommandMessage, Payload)>> = match self
            .messages.as_mut()
            .poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                  return Poll::Ready(Some(Err(Error::Connection(ConnectionError::Disconnected))));
                }
                Poll::Ready(Some(RawMessage { command, payload })) => {
                    Some(command
                        .message
                        .and_then(move |msg| payload.map(move |payload| (msg, payload))))
                }
            };

        if message.is_some() {
            self.remaining_messages -= 1;
        }

        let (message, mut payload) = match message {
            Some(Some((message, payload))) => (message, payload),
            Some(None) => return Poll::Ready(Some(Err(Error::Consumer(ConsumerError::MissingPayload(format!(
                "Missing payload for message {:?}",
                message
            )))))),
            None => return Poll::Ready(None),
        };

        let compression = payload.metadata.compression;

        let payload = match compression {
          None | Some(0) => payload,
          // LZ4
          Some(1) => {
              #[cfg(not(feature = "lz4"))]
              {
                  return Poll::Ready(Some(Err(Error::Consumer(ConsumerError::Io(std::io::Error::new(
                                  std::io::ErrorKind::Other,
                                  "got a LZ4 compressed message but 'lz4' cargo feature is deactivated"))))));
              }

              #[cfg(feature = "lz4")]
              {
                  use std::io::Read;

                  let mut decompressed_payload = Vec::new();
                  let mut decoder = lz4::Decoder::new(&payload.data[..]).map_err(ConsumerError::Io)?;
                  decoder.read_to_end(&mut decompressed_payload).map_err(ConsumerError::Io)?;

                  payload.data = decompressed_payload;
                  payload
              }
          },
          // zlib
          Some(2) => {
              #[cfg(not(feature = "flate2"))]
              {
                  return Poll::Ready(Some(Err(Error::Consumer(ConsumerError::Io(std::io::Error::new(
                                  std::io::ErrorKind::Other,
                                  "got a zlib compressed message but 'flate2' cargo feature is deactivated"))))));
              }

              #[cfg(feature = "flate2")]
              {
                  use std::io::Read;
                  use flate2::read::ZlibDecoder;

                  let mut d = ZlibDecoder::new(&payload.data[..]);
                  let mut decompressed_payload = Vec::new();
                  d.read_to_end(&mut decompressed_payload).map_err(ConsumerError::Io)?;

                  payload.data = decompressed_payload;
                  payload
              }
          },
          // zstd
          Some(3) => {
              #[cfg(not(feature = "zstd"))]
              {
                  return Poll::Ready(Some(Err(Error::Consumer(ConsumerError::Io(std::io::Error::new(
                                  std::io::ErrorKind::Other,
                                  "got a zstd compressed message but 'zstd' cargo feature is deactivated"))))));
              }

              #[cfg(feature = "zstd")]
              {
                  let decompressed_payload = zstd::decode_all(&payload.data[..]).map_err(ConsumerError::Io)?;

                  payload.data = decompressed_payload;
                  payload
              }
          },
          //Snappy
          Some(4) => {
              #[cfg(not(feature = "snap"))]
              {
                  return Poll::Ready(Some(Err(Error::Consumer(ConsumerError::Io(std::io::Error::new(
                                  std::io::ErrorKind::Other,
                                  "got a Snappy compressed message but 'snap' cargo feature is deactivated"))))));
              }

              #[cfg(feature = "snap")]
              {
                  use std::io::Read;

                  let mut decompressed_payload = Vec::new();
                  let mut decoder = snap::read::FrameDecoder::new(&payload.data[..]);
                  decoder.read_to_end(&mut decompressed_payload).map_err(ConsumerError::Io)?;

                  payload.data = decompressed_payload;
                  payload
              }
          },
          Some(i) => {
              error!("unknown compression type: {}", i);
              return Poll::Ready(None);
          }
        };

        match payload.metadata.num_messages_in_batch {
            Some(_) => {
                self.current_message =
                    Some(BatchedMessageIterator::new(message.message_id, payload)?);
                if let Poll::Ready(message) = self.poll_current_message() {
                    Poll::Ready(Some(Ok(message)))
                } else {
                    Poll::Pending
                }
            }
            None => Poll::Ready(Some(
                    Ok(self.create_message(message.message_id, payload)),
                    )),
        }
    }
}

pub struct Set<T>(T);

pub struct Unset;

pub struct ConsumerBuilder<'a, Topic, Subscription, SubscriptionType, Exe: Executor + ?Sized> {
    pulsar: &'a Pulsar<Exe>,
    topic: Topic,
    subscription: Subscription,
    subscription_type: SubscriptionType,
    consumer_id: Option<u64>,
    consumer_name: Option<String>,
    batch_size: Option<u32>,
    unacked_message_resend_delay: Option<Duration>,
    consumer_options: Option<ConsumerOptions>,

    // Currently only used for multi-topic
    namespace: Option<String>,
    topic_refresh: Option<Duration>,
}

impl<'a, Exe: Executor + ?Sized> ConsumerBuilder<'a, Unset, Unset, Unset, Exe> {
    pub fn new(pulsar: &'a Pulsar<Exe>) -> Self {
        ConsumerBuilder {
            pulsar,
            topic: Unset,
            subscription: Unset,
            subscription_type: Unset,
            consumer_id: None,
            consumer_name: None,
            batch_size: None,
            //TODO what should this default to? None seems incorrect..
            unacked_message_resend_delay: None,
            consumer_options: None,
            namespace: None,
            topic_refresh: None,
        }
    }
}

impl<'a, Subscription, SubscriptionType, Exe: Executor + ?Sized>
    ConsumerBuilder<'a, Unset, Subscription, SubscriptionType, Exe>
{
    pub fn with_topic<S: Into<String>>(
        self,
        topic: S,
    ) -> ConsumerBuilder<'a, Set<String>, Subscription, SubscriptionType, Exe> {
        ConsumerBuilder {
            pulsar: self.pulsar,
            topic: Set(topic.into()),
            subscription: self.subscription,
            subscription_type: self.subscription_type,
            consumer_id: self.consumer_id,
            consumer_name: self.consumer_name,
            consumer_options: self.consumer_options,
            batch_size: self.batch_size,
            namespace: self.namespace,
            topic_refresh: self.topic_refresh,
            unacked_message_resend_delay: self.unacked_message_resend_delay,
        }
    }

    pub fn multi_topic(
        self,
        regex: Regex,
    ) -> ConsumerBuilder<'a, Set<Regex>, Subscription, SubscriptionType, Exe> {
        ConsumerBuilder {
            pulsar: self.pulsar,
            topic: Set(regex),
            subscription: self.subscription,
            subscription_type: self.subscription_type,
            consumer_id: self.consumer_id,
            consumer_name: self.consumer_name,
            consumer_options: self.consumer_options,
            batch_size: self.batch_size,
            namespace: self.namespace,
            topic_refresh: self.topic_refresh,
            unacked_message_resend_delay: self.unacked_message_resend_delay,
        }
    }
}

impl<'a, Topic, SubscriptionType, Exe: Executor + ?Sized> ConsumerBuilder<'a, Topic, Unset, SubscriptionType, Exe> {
    pub fn with_subscription<S: Into<String>>(
        self,
        subscription: S,
    ) -> ConsumerBuilder<'a, Topic, Set<String>, SubscriptionType, Exe> {
        ConsumerBuilder {
            pulsar: self.pulsar,
            subscription: Set(subscription.into()),
            topic: self.topic,
            subscription_type: self.subscription_type,
            consumer_id: self.consumer_id,
            consumer_name: self.consumer_name,
            consumer_options: self.consumer_options,
            batch_size: self.batch_size,
            namespace: self.namespace,
            topic_refresh: self.topic_refresh,
            unacked_message_resend_delay: self.unacked_message_resend_delay,
        }
    }
}

impl<'a, Topic, Subscription, Exe: Executor + ?Sized> ConsumerBuilder<'a, Topic, Subscription, Unset, Exe> {
    pub fn with_subscription_type(
        self,
        subscription_type: SubType,
    ) -> ConsumerBuilder<'a, Topic, Subscription, Set<SubType>, Exe> {
        ConsumerBuilder {
            pulsar: self.pulsar,
            subscription_type: Set(subscription_type),
            topic: self.topic,
            subscription: self.subscription,
            consumer_id: self.consumer_id,
            consumer_name: self.consumer_name,
            consumer_options: self.consumer_options,
            batch_size: self.batch_size,
            namespace: self.namespace,
            topic_refresh: self.topic_refresh,
            unacked_message_resend_delay: self.unacked_message_resend_delay,
        }
    }
}

impl<'a, Subscription, SubscriptionType, Exe: Executor + ?Sized>
    ConsumerBuilder<'a, Set<Regex>, Subscription, SubscriptionType, Exe>
{
    pub fn with_namespace<S: Into<String>>(
        self,
        namespace: S,
    ) -> ConsumerBuilder<'a, Set<Regex>, Subscription, SubscriptionType, Exe> {
        ConsumerBuilder {
            pulsar: self.pulsar,
            topic: self.topic,
            subscription: self.subscription,
            subscription_type: self.subscription_type,
            consumer_name: self.consumer_name,
            consumer_id: self.consumer_id,
            consumer_options: self.consumer_options,
            batch_size: self.batch_size,
            namespace: Some(namespace.into()),
            topic_refresh: self.topic_refresh,
            unacked_message_resend_delay: self.unacked_message_resend_delay,
        }
    }

    pub fn with_topic_refresh(
        self,
        refresh_interval: Duration,
    ) -> ConsumerBuilder<'a, Set<Regex>, Subscription, SubscriptionType, Exe> {
        ConsumerBuilder {
            pulsar: self.pulsar,
            topic: self.topic,
            subscription: self.subscription,
            subscription_type: self.subscription_type,
            consumer_name: self.consumer_name,
            consumer_id: self.consumer_id,
            consumer_options: self.consumer_options,
            batch_size: self.batch_size,
            namespace: self.namespace,
            topic_refresh: Some(refresh_interval),
            unacked_message_resend_delay: self.unacked_message_resend_delay,
        }
    }
}

impl<'a, Topic, Subscription, SubscriptionType, Exe: Executor + ?Sized>
    ConsumerBuilder<'a, Topic, Subscription, SubscriptionType, Exe>
{
    pub fn with_consumer_id(
        mut self,
        consumer_id: u64,
    ) -> ConsumerBuilder<'a, Topic, Subscription, SubscriptionType, Exe> {
        self.consumer_id = Some(consumer_id);
        self
    }

    pub fn with_consumer_name<S: Into<String>>(
        mut self,
        consumer_name: S,
    ) -> ConsumerBuilder<'a, Topic, Subscription, SubscriptionType, Exe> {
        self.consumer_name = Some(consumer_name.into());
        self
    }

    pub fn with_batch_size(
        mut self,
        batch_size: u32,
    ) -> ConsumerBuilder<'a, Topic, Subscription, SubscriptionType, Exe> {
        self.batch_size = Some(batch_size);
        self
    }

    pub fn with_options(
        mut self,
        options: ConsumerOptions,
    ) -> ConsumerBuilder<'a, Topic, Subscription, SubscriptionType, Exe> {
        self.consumer_options = Some(options);
        self
    }

    /// The time after which a message is dropped without being acknowledged or nacked
    /// that the message is resent. If `None`, messages will only be resent when a
    /// consumer disconnects with pending unacknowledged messages.
    pub fn with_unacked_message_resend_delay(mut self, delay: Option<Duration>) -> Self {
        self.unacked_message_resend_delay = delay;
        self
    }
}

impl<'a, Exe: Executor> ConsumerBuilder<'a, Set<String>, Set<String>, Set<SubType>, Exe> {
    pub async fn build<T: DeserializeMessage>(self) -> Result<Consumer<T>, Error> {
        let ConsumerBuilder {
            pulsar,
            topic: Set(topic),
            subscription: Set(subscription),
            subscription_type: Set(sub_type),
            consumer_id,
            consumer_name,
            consumer_options,
            batch_size,
            unacked_message_resend_delay,
            ..
        } = self;

        pulsar.create_consumer(
            topic,
            subscription,
            sub_type,
            batch_size,
            consumer_name,
            consumer_id,
            unacked_message_resend_delay,
            consumer_options.unwrap_or_else(ConsumerOptions::default),
        ).await
    }
}

impl<'a, Exe: Executor> ConsumerBuilder<'a, Set<Regex>, Set<String>, Set<SubType>, Exe> {
    pub fn build<T: DeserializeMessage>(self) -> MultiTopicConsumer<T, Exe> {
        let ConsumerBuilder {
            pulsar,
            topic: Set(topic),
            subscription: Set(subscription),
            subscription_type: Set(sub_type),
            consumer_id,
            consumer_name,
            batch_size,
            topic_refresh,
            namespace,
            unacked_message_resend_delay,
            ..
        } = self;
        if consumer_id.is_some() {
            warn!("Multi-topic consumers cannot have a set consumer ID; ignoring.");
        }
        if consumer_name.is_some() {
            warn!("Consumer name not currently supported for Multi-topic consumers; ignoring.");
        }
        if batch_size.is_some() {
            warn!("Batch size not currently supported for Multi-topic consumers; ignoring.");
        }
        let namespace = namespace.unwrap_or_else(|| "public/default".to_owned());
        let topic_refresh = topic_refresh.unwrap_or_else(|| Duration::from_secs(30));

        pulsar.create_multi_topic_consumer(
            topic,
            subscription,
            namespace,
            sub_type,
            topic_refresh,
            unacked_message_resend_delay,
            ConsumerOptions::default(),
        )
    }
}

/// Details about the current state of the Consumer
#[derive(Debug, Clone)]
pub struct ConsumerState {
    pub connected_topics: Vec<String>,
    pub last_message_received: Option<DateTime<Utc>>,
    pub messages_received: u64,
}

pub struct MultiTopicConsumer<T: DeserializeMessage, Exe: Executor> {
    namespace: String,
    topic_regex: Regex,
    pulsar: Pulsar<Exe>,
    unacked_message_resend_delay: Option<Duration>,
    consumers: BTreeMap<String, Pin<Box<Consumer<T>>>>,
    topics: VecDeque<String>,
    new_consumers: Option<Pin<Box<dyn Future<Output = Result<Vec<Consumer<T>>, Error>> + Send>>>,
    refresh: Pin<Box<dyn Stream<Item = ()> + Send>>,
    subscription: String,
    sub_type: SubType,
    options: ConsumerOptions,
    last_message_received: Option<DateTime<Utc>>,
    messages_received: u64,
    state_streams: Vec<UnboundedSender<ConsumerState>>,
}

impl<T: DeserializeMessage, Exe: Executor> MultiTopicConsumer<T, Exe> {
    pub fn new<S1, S2>(
        pulsar: Pulsar<Exe>,
        namespace: S1,
        topic_regex: Regex,
        subscription: S2,
        sub_type: SubType,
        topic_refresh: Duration,
        unacked_message_resend_delay: Option<Duration>,
        options: ConsumerOptions,
    ) -> Self
    where
        S1: Into<String>,
        S2: Into<String>,
    {
        MultiTopicConsumer {
            namespace: namespace.into(),
            topic_regex,
            pulsar,
            unacked_message_resend_delay,
            consumers: BTreeMap::new(),
            topics: VecDeque::new(),
            new_consumers: None,
            refresh: Box::pin(
                Exe::interval(topic_refresh)
                    .map(drop)
                    //.map_err(|e| panic!("error creating referesh timer: {}", e)),
            ),
            subscription: subscription.into(),
            sub_type,
            last_message_received: None,
            messages_received: 0,
            state_streams: vec![],
            options,
        }
    }

    pub fn start_state_stream(&mut self) -> impl Stream<Item = ConsumerState> {
        let (tx, rx) = unbounded();
        self.state_streams.push(tx);
        rx
    }

    fn send_state(&mut self) {
        if !self.state_streams.is_empty() {
            let state = ConsumerState {
                connected_topics: self.consumers.keys().cloned().collect(),
                last_message_received: self.last_message_received,
                messages_received: self.messages_received,
            };
            self.state_streams
                .retain(|s| s.unbounded_send(state.clone()).is_ok());
        }
    }

    fn record_message(&mut self) {
        self.last_message_received = Some(Utc::now());
        self.messages_received += 1;
        self.send_state();
    }

    fn add_consumers<I: IntoIterator<Item = Consumer<T>>>(&mut self, consumers: I) {
        for consumer in consumers {
            let topic = consumer.topic().to_owned();
            self.consumers.insert(topic.clone(), Box::pin(consumer));
            self.topics.push_back(topic);
        }

        self.send_state();
    }

    fn remove_consumers(&mut self, topics: &[String]) {
        self.topics.retain(|t| !topics.contains(t));
        for topic in topics {
            self.consumers.remove(topic);
        }
        self.send_state();
    }

    pub fn ack(&self, msg: &Message<T>) -> Result<(), ConnectionError> {
        if let Some(c) = self.consumers.get(&msg.topic) {
            c.ack(&msg)
        } else {
            Err(ConnectionError::Unexpected(format!("no consumer for topic {}", msg.topic)))
        }
    }
}

pub struct Message<T> {
    pub topic: String,
    pub payload: Payload,
    message_id: MessageData,
    _phantom: PhantomData<T>,
}

impl<T: DeserializeMessage> Message<T> {
  pub fn deserialize(&self) -> T::Output {
      T::deserialize_message(&self.payload)
  }
}

impl<T: DeserializeMessage, Exe: Executor> Debug for MultiTopicConsumer<T, Exe> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "MultiTopicConsumer({:?}, {:?})",
            &self.namespace, &self.topic_regex
        )
    }
}

impl<T: 'static + DeserializeMessage, Exe: Executor> Stream for MultiTopicConsumer<T, Exe> {
    type Item = Result<Message<T>, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(mut new_consumers) = self.new_consumers.take() {
            match new_consumers.as_mut().poll(cx) {
                Poll::Ready(Ok(new_consumers)) => {
                    self.add_consumers(new_consumers);
                }
                Poll::Pending => {
                    self.new_consumers = Some(new_consumers);
                }
                Poll::Ready(Err(e)) => {
                    error!("Error creating pulsar consumers: {}", e);
                    // don't return error here; could be intermittent connection failure and we want
                    // to retry
                }
            }
        }

        if let Poll::Ready(Some(_)) = self.refresh.as_mut().poll_next(cx) {
            let regex = self.topic_regex.clone();
            let pulsar = self.pulsar.clone();
            let namespace = self.namespace.clone();
            let subscription = self.subscription.clone();
            let sub_type = self.sub_type;
            let existing_topics: BTreeSet<String> = self.consumers.keys().cloned().collect();
            let options = self.options.clone();
            let unacked_message_resend_delay = self.unacked_message_resend_delay;

            let new_consumers = Box::pin(async move {
                let topics: Vec<String> = pulsar
                    .get_topics_of_namespace(namespace.clone(), proto::get_topics::Mode::All).await?;
                trace!("fetched topics: {:?}", &topics);

                let mut v = vec![];
                for topic in topics
                             .into_iter()
                             .filter(move |topic| {
                                 !existing_topics.contains(topic)
                                     && regex.is_match(topic.as_str())
                             }) {
                    trace!("creating consumer for topic {}", topic);
                    //let pulsar = pulsar.clone();
                    let subscription = subscription.clone();
                    v.push(pulsar.create_consumer(
                        topic,
                        subscription,
                        sub_type,
                        None,
                        None,
                        None,
                        unacked_message_resend_delay,
                        options.clone(),
                    ));
                }

                try_join_all(v).await
            });
            self.new_consumers = Some(new_consumers);
            return self.poll_next(cx);
        }

        let mut topics_to_remove = Vec::new();
        let mut result = None;
        for _ in 0..self.topics.len() {
            if result.is_some() {
                break;
            }
            let topic = self.topics.pop_front().unwrap();
            if let Some(item) = self.consumers.get_mut(&topic).map(|c| c.as_mut().poll_next(cx)) {
                match item {
                    Poll::Pending => {}
                    Poll::Ready(Some(Ok(msg))) => result = Some(msg),
                    Poll::Ready(None) => {
                        error!("Unexpected end of stream for pulsar topic {}", &topic);
                        topics_to_remove.push(topic.clone());
                    }
                    Poll::Ready(Some(Err(e))) => {
                        error!(
                            "Unexpected error consuming from pulsar topic {}: {}",
                            &topic, e
                        );
                        topics_to_remove.push(topic.clone());
                    }
                }
            } else {
                eprintln!("BUG: Missing consumer for topic {}", &topic);
            }
            self.topics.push_back(topic);
        }
        self.remove_consumers(&topics_to_remove);
        if let Some(result) = result {
            self.record_message();
            return Poll::Ready(Some(Ok(result)));
        }

        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;
    use std::thread;

    use regex::Regex;
    #[cfg(feature = "tokio-runtime")]
    use tokio::runtime::Runtime;
    use log::{LevelFilter};

    use crate::{producer, Pulsar, SerializeMessage, tests::TEST_LOGGER};
    #[cfg(feature = "tokio-runtime")]
    use crate::executor::TokioExecutor;

    use super::*;

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
    pub struct TestData {
        topic: String,
        msg: u32,
    }

    impl SerializeMessage for TestData {
        fn serialize_message(input: &Self) -> Result<producer::Message, Error> {
            let payload = serde_json::to_vec(input).map_err(|e| Error::Custom(e.to_string()))?;
            Ok(producer::Message {
                payload,
                ..Default::default()
            })
        }
    }

    impl DeserializeMessage for TestData {
        type Output = Result<TestData, serde_json::Error>;

        fn deserialize_message(payload: &Payload) -> Self::Output {
            serde_json::from_slice(&payload.data)
        }
    }

    #[test]
    #[ignore]
    #[cfg(feature = "tokio-runtime")]
    fn multi_consumer() {
        let _ = log::set_logger(&TEST_LOGGER);
        let _ = log::set_max_level(LevelFilter::Debug);
        let addr = "pulsar://127.0.0.1:6650";
        let rt = Runtime::new().unwrap();

        let namespace = "public/default";
        let topic1 = "mt_test_a";
        let topic2 = "mt_test_b";

        let data1 = TestData {
            topic: "a".to_owned(),
            msg: 1,
        };
        let data2 = TestData {
            topic: "a".to_owned(),
            msg: 2,
        };
        let data3 = TestData {
            topic: "b".to_owned(),
            msg: 1,
        };
        let data4 = TestData {
            topic: "b".to_owned(),
            msg: 2,
        };

        let error: Arc<Mutex<Option<Error>>> = Arc::new(Mutex::new(None));
        let successes = Arc::new(AtomicUsize::new(0));
        let err = error.clone();

        let succ = successes.clone();

        let f = async move {
            let client: Pulsar<TokioExecutor> = Pulsar::new(addr, None).await.unwrap();
            let producer = client.producer(None);

            let send_start = Utc::now();
            producer.send(topic1, data1.clone()).await.unwrap();
            producer.send(topic1, data2.clone()).await.unwrap();
            producer.send(topic2, data3.clone()).await.unwrap();
            producer.send(topic2, data4.clone()).await.unwrap();

            let data = vec![data1, data2, data3, data4];

            let mut consumer: MultiTopicConsumer<TestData, _> = client
                .consumer()
                .multi_topic(Regex::new("mt_test_[ab]").unwrap())
                .with_namespace(namespace)
                .with_subscription("test_sub")
                .with_subscription_type(SubType::Shared)
                .with_topic_refresh(Duration::from_secs(1))
                .build();

            let consumer_state = consumer.start_state_stream();

            let mut counter = 0usize;
            while let Some(res) = consumer.next().await {
                match res {
                    Ok(message) => {
                        consumer.ack(&message);
                        let msg = message.deserialize().unwrap();
                        if !data.contains(&msg) {
                            panic!("Unexpected message: {:?}", &msg);
                        } else {
                            succ.fetch_add(1, Ordering::Relaxed);
                        }
                    },
                    Err(e) => {
                        let err = err.clone();
                        let mut error = err.lock().unwrap();
                        *error = Some(e);
                    },
                }

                counter += 1;
                if counter == 4 {
                    break;
                }
            }

            let consumer_state: Vec<ConsumerState> = consumer_state.collect::<Vec<ConsumerState>>().await;
            let latest_state = consumer_state.last().unwrap();
            assert!(latest_state.messages_received >= 4);
            assert!(latest_state.connected_topics.len() >= 2);
            assert!(latest_state.last_message_received.unwrap() >= send_start);
        };

        rt.spawn(f);

        let start = Instant::now();
        loop {
            let success_count = successes.load(Ordering::Relaxed);
            if success_count == 4 {
                break;
            } else if start.elapsed() > Duration::from_secs(3) {
                panic!("Messages not received within timeout");
            }
            thread::sleep(Duration::from_millis(100));
        }

    }

    #[test]
    #[cfg(feature = "tokio-runtime")]
    fn consumer_dropped_with_lingering_acks() {
        use rand::{Rng, distributions::Alphanumeric};
        let _ = log::set_logger(&TEST_LOGGER);
        let _ = log::set_max_level(LevelFilter::Debug);
        let addr = "pulsar://127.0.0.1:6650";
        let mut rt = Runtime::new().unwrap();

        let topic = "issue_51";

        let f = async move {
            let client: Pulsar<TokioExecutor> = Pulsar::new(addr, None).await.unwrap();

            let message = TestData {
                topic: std::iter::repeat(()).map(|()| rand::thread_rng().sample(Alphanumeric))
                    .take(8).collect(),
                msg: 1,
            };

            {
                let producer = client.producer(None);

                producer.send(topic, message.clone()).await.unwrap();
                println!("producer sends done");
            }

            {
                println!("creating consumer");
                let mut consumer: Consumer<TestData> = client
                    .consumer()
                    .with_topic(topic)
                    .with_subscription("dropped_ack")
                    .with_subscription_type(SubType::Shared)
                    // get earliest messages
                    .with_options(ConsumerOptions { initial_position: Some(1), ..Default::default() })
                    .build().await.unwrap();

                println!("created consumer");

                //consumer.next().await
                let msg = consumer.next().await.unwrap().unwrap();
                println!("got message: {:?}", msg.payload);
                assert_eq!(message, msg.deserialize().unwrap(), "we probably receive a message from a previous run of the test");
                consumer.ack(&msg);
            }

            {
                println!("creating second consumer. The message should have been acked");
                let mut consumer: Consumer<TestData> = client
                    .consumer()
                    .with_topic(topic)
                    .with_subscription("dropped_ack")
                    .with_subscription_type(SubType::Shared)
                    .with_options(ConsumerOptions { initial_position: Some(1), ..Default::default() })
                    .build().await.unwrap();

                println!("created second consumer");

                // the message has already been acked, so we should not receive anything
                let res: Result<_, tokio::time::Elapsed> = tokio::time::timeout(Duration::from_secs(1), consumer.next()).await;
                let is_err = res.is_err();
                if let Ok(val) = res {
                    let msg = val.unwrap().unwrap();
                    println!("got message: {:?}", msg.payload);
                    // cleanup for the next test
                    consumer.ack(&msg);
                    // we should not receive a different message anyway
                    assert_eq!(message, msg.deserialize().unwrap());
                }

                assert!(is_err, "waiting for a message should have timed out, since we already acknowledged the only message in the queue");
            }
        };

        rt.block_on(f);
    }
}
