// Copyright 2021, The Tremor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// #![cfg_attr(coverage, no_coverage)] // This is for benchmarking and testing

use std::time::Duration;

use crate::connectors::prelude::*;
use crate::system::{ShutdownMode, World};
use async_std::io::prelude::BufReadExt;
use async_std::stream::StreamExt;
use async_std::{fs::File, io};
use tremor_common::asy::file::open;

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// path to file to load data from
    path: Option<String>,
    // timeout in nanoseconds
    #[serde(default = "default_timeout")]
    timeout: u64,
    // only expect the latest event to be acked, the earliest to be failed
    #[serde(default = "default_false")]
    expect_batched: bool,
}

/// 10 seconds
fn default_timeout() -> u64 {
    10_000_000_000
}

impl ConfigImpl for Config {}

#[derive(Debug)]
pub(crate) struct Builder {
    world: World,
}

impl Builder {
    pub(crate) fn new(world: World) -> Self {
        Self { world }
    }
}

#[async_trait::async_trait()]
impl ConnectorBuilder for Builder {
    fn connector_type(&self) -> ConnectorType {
        "cb".into()
    }

    async fn build(&self, alias: &str, config: &ConnectorConfig) -> Result<Box<dyn Connector>> {
        if let Some(raw) = &config.config {
            let config = Config::new(raw)?;
            Ok(Box::new(Cb {
                config,
                world: self.world.clone(),
            }))
        } else {
            Err(ErrorKind::MissingConfiguration(alias.to_string()).into())
        }
    }
}

/// Testing connector for verifying correct CB Ack/Fail behaviour of the whole downstream pipeline/connectors
/// and for triggering custom cb (circuit breaker open/close) or gd (guaranteed delivery ack/fail) contraflow events.
///
/// Source: takes events from a file and expects at least one (or exactly one) ack or fail for each event.
/// Sink: expects a `"cb"` array or string in the event payload or metadata and reacts with the given event
///       (possible values: "ack", "fail", "open", "close", "trigger", "restore")
///
/// ### Notes:
///
/// * In case the connected pipeline drops events no ack or fail is received with the current runtime.
/// * In case the pipeline branches off, it copies the event and it reaches two offramps, we might receive more than 1 ack or fail for an event with the current runtime.
pub struct Cb {
    config: Config,
    world: World,
}

#[async_trait::async_trait()]
impl Connector for Cb {
    fn codec_requirements(&self) -> CodecReq {
        CodecReq::Optional("json")
    }

    async fn create_source(
        &mut self,
        source_context: SourceContext,
        builder: SourceManagerBuilder,
    ) -> Result<Option<SourceAddr>> {
        let source =
            CbSource::new(&self.config, source_context.alias(), self.world.clone()).await?;
        let source_addr = builder.spawn(source, source_context)?;
        Ok(Some(source_addr))
    }

    async fn create_sink(
        &mut self,
        sink_context: SinkContext,
        builder: SinkManagerBuilder,
    ) -> Result<Option<SinkAddr>> {
        let sink = CbSink {};
        let sink_addr = builder.spawn(sink, sink_context)?;
        Ok(Some(sink_addr))
    }
}

struct CbSink {}

impl CbSink {
    const CB: &'static str = "cb";
}

#[async_trait::async_trait()]
impl Sink for CbSink {
    async fn on_event(
        &mut self,
        _input: &str,
        event: Event,
        _ctx: &SinkContext,
        _serializer: &mut EventSerializer,
        _start: u64,
    ) -> Result<SinkReply> {
        for (value, meta) in event.value_meta_iter() {
            if let Some(cb) = meta.get(Self::CB).or_else(|| value.get(Self::CB)) {
                let cb_cmds = if let Some(array) = cb.as_array() {
                    array
                        .iter()
                        .filter_map(|v| v.as_str().map(ToString::to_string))
                        .collect()
                } else if let Some(str) = cb.as_str() {
                    vec![str.to_string()]
                } else {
                    vec![]
                };

                // Acknowledgement tracking
                let ack = if cb_cmds.contains(&"ack".to_string()) {
                    SinkAck::Ack
                } else if cb_cmds.contains(&"fail".to_string()) {
                    SinkAck::Fail
                } else {
                    SinkAck::None
                };

                // Circuit breaker tracking
                let cb = if cb_cmds.contains(&"close".to_string())
                    || cb_cmds.contains(&"trigger".to_string())
                {
                    CbAction::Close
                } else if cb_cmds.contains(&"open".to_string())
                    || cb_cmds.contains(&"restore".to_string())
                {
                    CbAction::Open
                } else {
                    CbAction::None
                };
                return Ok(SinkReply { ack, cb });
            }
        }
        Ok(SinkReply::NONE)
    }

    fn auto_ack(&self) -> bool {
        false
    }
}

#[derive(Default, Debug)]
struct ReceivedCbs {
    ack: Vec<u64>,  // collect ids of acks
    fail: Vec<u64>, // collect ids of fails
    trigger: u64,   // counter
    restore: u64,   // counter
}

impl ReceivedCbs {
    fn count(&self) -> usize {
        self.ack.len() + self.fail.len()
    }

    fn max(&self) -> Option<u64> {
        self.ack
            .iter()
            .copied()
            .max()
            .max(self.fail.iter().copied().max())
    }
}

#[derive(Debug)]
struct CbSource {
    file: io::Lines<io::BufReader<File>>,
    num_sent: usize,
    last_sent: u64,
    received_cbs: ReceivedCbs,
    finished: bool,
    config: Config,
    origin_uri: EventOriginUri,
    world: World,
}

impl CbSource {
    fn did_receive_all(&self) -> bool {
        let all_received = if self.config.expect_batched {
            self.received_cbs
                .max()
                .map(|m| m == self.last_sent)
                .unwrap_or_default()
        } else {
            self.received_cbs.count() == self.num_sent
        };
        self.finished && all_received
    }
    async fn new(config: &Config, alias: &str, world: World) -> Result<Self> {
        if let Some(path) = config.path.as_ref() {
            let file = open(path).await?;
            Ok(Self {
                file: io::BufReader::new(file).lines(),
                num_sent: 0,
                last_sent: 0,
                received_cbs: ReceivedCbs::default(),
                finished: false,
                config: config.clone(),
                origin_uri: EventOriginUri {
                    scheme: String::from("tremor-cb"),
                    host: hostname(),
                    ..EventOriginUri::default()
                },
                world,
            })
        } else {
            Err(ErrorKind::InvalidConnectorDefinition(
                alias.to_string(),
                String::from("Missing path key."),
            )
            .into())
        }
    }
}

#[async_trait::async_trait()]
impl Source for CbSource {
    async fn pull_data(&mut self, pull_id: &mut u64, _ctx: &SourceContext) -> Result<SourceReply> {
        if let Some(line) = self.file.next().await {
            self.num_sent += 1;
            self.last_sent = self.last_sent.max(*pull_id);

            Ok(SourceReply::Data {
                data: line?.into_bytes(),
                meta: None,
                stream: Some(DEFAULT_STREAM_ID),
                port: None,
                origin_uri: self.origin_uri.clone(),
                codec_overwrite: None,
            })
        } else if self.finished {
            let world = self.world.clone();

            if self.config.timeout > 0 && !self.did_receive_all() {
                async_std::task::sleep(Duration::from_nanos(self.config.timeout)).await;
            }

            if self.did_receive_all() {
                eprintln!("All required CB events received.");
                eprintln!("Got acks: {:?}", self.received_cbs.ack);
                eprintln!("Got fails: {:?}", self.received_cbs.fail);
            } else {
                // report failures to stderr and exit with 1
                eprintln!("Expected CB events up to id {}.", self.last_sent);
                eprintln!("Got acks: {:?}", self.received_cbs.ack);
                eprintln!("Got fails: {:?}", self.received_cbs.fail);
            }
            async_std::task::spawn::<_, Result<()>>(async move {
                world.stop(ShutdownMode::Graceful).await?;
                Ok(())
            });

            Ok(SourceReply::Finished)
        } else {
            self.finished = true;
            Ok(SourceReply::EndStream {
                stream: DEFAULT_STREAM_ID,
                origin_uri: self.origin_uri.clone(),
                meta: None,
            })
        }
    }

    async fn on_cb_close(&mut self, _ctx: &SourceContext) -> Result<()> {
        self.received_cbs.trigger += 1;
        Ok(())
    }
    async fn on_cb_open(&mut self, _ctx: &SourceContext) -> Result<()> {
        self.received_cbs.restore += 1;
        Ok(())
    }

    async fn ack(&mut self, _stream_id: u64, pull_id: u64, _ctx: &SourceContext) -> Result<()> {
        self.received_cbs.ack.push(pull_id);
        Ok(())
    }

    async fn fail(&mut self, _stream_id: u64, pull_id: u64, _ctx: &SourceContext) -> Result<()> {
        self.received_cbs.fail.push(pull_id);
        Ok(())
    }

    fn is_transactional(&self) -> bool {
        true
    }

    fn asynchronous(&self) -> bool {
        false
    }
}
