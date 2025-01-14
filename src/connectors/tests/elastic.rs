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
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or imelied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::time::{Duration, Instant};

use super::ConnectorHarness;
use crate::connectors::impls::elastic;
use crate::errors::{Error, Result};
use elasticsearch::{http::transport::Transport, Elasticsearch};
use futures::TryFutureExt;
use testcontainers::{clients, images::generic::GenericImage, RunnableImage};
use tremor_common::ports::IN;
use tremor_pipeline::{CbAction, Event, EventId};
use tremor_value::{literal, value::StaticValue};
use value_trait::{Mutable, Value, ValueAccess};

const ELASTICSEARCH_VERSION: &str = "7.17.2";

#[async_std::test]
async fn connector_elastic() -> Result<()> {
    let _ = env_logger::try_init();

    let docker = clients::Cli::default();
    let port = super::free_port::find_free_tcp_port().await?;
    let image = RunnableImage::from(
        GenericImage::new("elasticsearch", ELASTICSEARCH_VERSION)
            .with_env_var("discovery.type", "single-node")
            .with_env_var("ES_JAVA_OPTS", "-Xms256m -Xmx256m"),
    )
    .with_mapped_port((port, 9200_u16));

    let container = docker.run(image);
    let port = container.get_host_port(9200);

    // wait for the image to be reachable
    let elastic = Elasticsearch::new(Transport::single_node(
        format!("http://127.0.0.1:{port}").as_str(),
    )?);
    let wait_for = Duration::from_secs(60); // that shit takes a while
    let start = Instant::now();
    while let Err(e) = elastic
        .cluster()
        .health(elasticsearch::cluster::ClusterHealthParts::None)
        .send()
        .and_then(|r| r.json::<StaticValue>())
        .await
    {
        if start.elapsed() > wait_for {
            return Err(
                Error::from(e).chain_err(|| "Waiting for elasticsearch container timed out.")
            );
        }
        async_std::task::sleep(Duration::from_secs(1)).await;
    }

    let connector_config = literal!({
        "reconnect": {
            "retry": {
                "interval_ms": 1000,
                "max_retries": 10
            }
        },
        "config": {
            "nodes": [
                format!("http://127.0.0.1:{port}")
            ]
        }
    });
    let harness = ConnectorHarness::new(
        function_name!(),
        &elastic::Builder::default(),
        &connector_config,
    )
    .await?;
    let out = harness.out().expect("No pipe connected to port OUT");
    let err = harness.err().expect("No pipe connected to port ERR");
    let in_pipe = harness.get_pipe(IN).expect("No pipe connected to port IN");
    harness.start().await?;
    harness.wait_for_connected().await?;
    harness.consume_initial_sink_contraflow().await?;

    let data = literal!({
        "field1": 12.5,
        "field2": "string",
        "field3": [true, false],
        "field4": {
            "nested": true
        }
    });
    let meta = literal!({
        "elastic": {
            "_index": "my_index",
            "_id": "123",
            "action": "index"
        },
        "correlation": {
            "snot": ["badger", 42, false]
        }
    });
    let event_not_batched = Event {
        id: EventId::default(),
        data: (data, meta).into(),
        transactional: false,
        ..Event::default()
    };
    harness.send_to_sink(event_not_batched, IN).await?;
    let err_events = err.get_events()?;
    assert!(err_events.is_empty(), "Received err msgs: {:?}", err_events);
    let event = out.get_event().await?;
    assert_eq!(
        &literal!({
            "elastic": {
                "_id": "123",
                "_index": "my_index",
                "_type": "_doc",
                "version": 1,
                "action": "index",
                "success": true
            },
            "correlation": {
                "snot": ["badger", 42, false]
            }
        }),
        event.data.suffix().meta()
    );
    assert_eq!(
        &literal!({
            "index": {
                "_primary_term": 1,
                "_shards": {
                    "total": 2,
                    "successful": 1,
                    "failed": 0,
                },
                "_type": "_doc",
                "_index": "my_index",
                "result": "created",
                "_id": "123",
                "_seq_no": 0,
                "status": 201,
                "_version": 1
            }
        }),
        event.data.suffix().value()
    );

    // upsert

    let data = literal!({
        "doc": {
            "field1": 13.4,
            "field2": "strong",
            "field3": [false, true],
            "field4": {
                "nested": false
            }
        },
        "doc_as_upsert": true,
    });
    let meta = literal!({
        "elastic": {
            "_index": "my_index",
            "_id": "1234",
            "action": "update",
            "raw_payload": true
        },
    });
    let event_not_batched = Event {
        id: EventId::default(),
        data: (data, meta).into(),
        transactional: false,
        ..Event::default()
    };
    harness.send_to_sink(event_not_batched, IN).await?;
    let err_events = err.get_events()?;
    assert!(err_events.is_empty(), "Received err msgs: {:?}", err_events);
    let event = out.get_event().await?;
    assert_eq!(
        &literal!({
            "elastic": {
                "_id": "1234",
                "_index": "my_index",
                "_type": "_doc",
                "version": 1,
                "action": "update",
                "success": true
            },
        }),
        event.data.suffix().meta()
    );
    assert_eq!(
        &literal!({
            "update": {
                "_primary_term": 1,
                "_shards": {
                    "total": 2,
                    "successful": 1,
                    "failed": 0,
                },
                "_type": "_doc",
                "_index": "my_index",
                "result": "created",
                "_id": "1234",
                "_seq_no": 1,
                "status": 201,
                "_version": 1
            }
        }),
        event.data.suffix().value()
    );

    let batched_data = literal!([{
            "data": {
                "value": {
                    "field1": 0.1,
                    "field2": "another_string",
                    "field3": [],
                },
                "meta": {}
            }
        },
        {
            "data": {
                "value": {
                    "field3": 12,
                    "field4": {
                        "nested": false,
                        "actually": "no"
                    }
                },
                "meta": {
                    "elastic": {
                        "action": "update",
                        "_id": "123",
                    }
                }
            }
           },
        {
            "data": {
                "value": {},
                "meta": {
                    "correlation": "snot"
                }
            }
        }
    ]);
    let batched_meta = literal!({
        "elastic": {
            "_index": "my_index",
            "_type": "_doc"
        }
    });
    let batched_id = EventId::new(0, 0, 1, 1);
    let event_batched = Event {
        id: batched_id.clone(),
        is_batch: true,
        transactional: true,
        data: (batched_data, batched_meta).into(),
        ..Event::default()
    };
    harness.send_to_sink(event_batched, IN).await?;
    let out_event1 = out.get_event().await?;
    let mut meta = out_event1.data.suffix().meta().clone_static();
    // remove _id
    assert!(meta
        .get_mut("elastic")
        .expect("no elastic in meta")
        .remove("_id")?
        .map(|v| v.is_str())
        .unwrap_or_default());
    assert_eq!(
        literal!({
            "elastic": {
                "_index": "my_index",
                "_type": "_doc",
                "version": 1,
                "action": "index",
                "success": true
            }
        }),
        meta
    );
    let mut data = out_event1.data.suffix().value().clone_static();
    // remove _id
    data.get_mut("index")
        .expect("no index in data")
        .remove("_id")?;
    assert_eq!(
        literal!({
            "index": {
                "_primary_term": 1,
                "_shards": {
                    "total": 2,
                    "successful": 1,
                    "failed": 0
                },
                "_type": "_doc",
                "_index": "my_index",
                "result": "created",
                "_seq_no": 2,
                "status": 201,
                "_version": 1
            }
        }),
        data
    );
    let err_event2 = err.get_event().await?;
    let meta = err_event2.data.suffix().meta();
    assert_eq!(
        literal!({
            "elastic": {
                "_id": "123",
                "_index": "my_index",
                "_type": "_doc",
                "action": "update",
                "success": false
            },
        }),
        meta
    );
    let data = err_event2.data.suffix().value();
    let data = data.get("update");
    let data = data.get("error");
    let reason = data.get_str("reason");
    assert_eq!(Some("failed to parse field [field3] of type [boolean] in document with id '123'. Preview of field's value: '12'"), reason);
    let out_event3 = out.get_event().await?;
    let mut meta = out_event3.data.suffix().meta().clone_static();
    // remove _id, as it is random
    assert!(meta
        .get_mut("elastic")
        .expect("No elastic in meta")
        .remove("_id")?
        .map(|s| s.is_str()) // at least check that it was a string
        .unwrap_or_default());

    assert_eq!(
        literal!({
            "elastic": {
                "success": true,
                "_index": "my_index",
                "_type": "_doc",
                "version": 1,
                "action": "index"
            },
            "correlation": "snot"
        }),
        meta
    );

    // a transactional event triggered a GD ACK
    let cf = in_pipe.get_contraflow().await?;
    assert_eq!(CbAction::Ack, cf.cb);
    assert_eq!(batched_id, cf.id);

    // check what happens when ES isnt reachable
    container.stop();

    let event = Event {
        id: EventId::new(0, 0, 2, 2),
        transactional: true,
        data: (
            literal!({}),
            literal!({
                "elastic": {
                    "_index": "my_index",
                    "action": "delete",
                    "_id": "123"
                },
                "correlation": [true, false]
            }),
        )
            .into(),
        ..Event::default()
    };
    harness.send_to_sink(event.clone(), IN).await?;
    let cf = in_pipe.get_contraflow().await?;
    assert_eq!(CbAction::Fail, cf.cb);
    let err_event = err.get_event().await?;
    let mut err_event_meta = err_event.data.suffix().meta().clone_static();
    let err_msg = err_event_meta.remove("error")?;
    let err_msg_str = err_msg.unwrap();
    let err = err_msg_str.as_str().unwrap();
    assert!(
        err.contains("tcp connect error: Connection refused"),
        "{err} does not contain Connection refused"
    );

    assert_eq!(
        literal!({
            "elastic": {
                "success": false
            },
            "correlation": [true, false]
        }),
        err_event_meta
    );

    let (out_events, err_events) = harness.stop().await?;
    assert!(out_events.is_empty());
    assert!(err_events.is_empty());
    // will rm the container
    drop(container);

    Ok(())
}
