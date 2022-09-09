use std::collections::HashMap;
use std::str::FromStr;
use std::thread;

use abscissa_core::clap::Parser;
use abscissa_core::config::Override;
use abscissa_core::{Command, FrameworkErrorKind, Runnable};
use actix::System;
use crossbeam_channel::unbounded;
use ibc_proto::google::protobuf::Any;
use tokio::runtime::Runtime as TokioRuntime;
use tracing::{debug, error, info, span, trace, warn, Level};

use cosmos_sdk_proto::cosmos::base::tendermint::v1beta1::service_client::ServiceClient as TmServiceClient;
use cosmos_sdk_proto::cosmos::base::tendermint::v1beta1::GetLatestBlockRequest;
use cosmos_sdk_proto::cosmos::tx::v1beta1::service_client::ServiceClient as TxServiceClient;
use cosmos_sdk_proto::cosmos::tx::v1beta1::{GetTxRequest, GetTxResponse};
use cosmos_sdk_proto::tendermint::abci::Event as TmEvent;
use ibc::core::ics02_client::height::Height as IbcHeight;
use ibc::core::ics04_channel as Channel;
use ibc::core::ics04_channel::events as ChannelEvents;
use ibc::core::ics04_channel::packet::Sequence;
use ibc::core::ics24_host::identifier::{ChainId, ChannelId, PortId};
use ibc::events::IbcEvent;
use ibc_proto::ibc::core::channel::v1::PacketSequence;
use ibc_relayer::chain::handle::{BaseChainHandle, ChainHandle};
use ibc_relayer::chain::tracking::{TrackedMsgs, TrackingId};
use ibc_relayer::config::Config;
use ibc_relayer::event::IbcEventWithHeight;
use ibc_relayer::link::error::LinkError;
use ibc_relayer::link::operational_data::{OperationalData, OperationalDataTarget, TrackedEvents};
use ibc_relayer::link::relay_sender::Submit;
use ibc_relayer::link::relay_sender::SubmitReply;
use ibc_relayer::link::relay_sender::SyncSender;
use ibc_relayer::link::{self, Link, LinkParameters, RelayPath};
use ibc_relayer::telemetry;

use crate::application::app_config;
use crate::cli_utils::spawn_chain_counterparty;
use crate::conclude::Output;
use crate::error::Error;

const SEND_PACKET_EVENT_KEY: &str = "send_packet";
/// `clear` subcommands
#[derive(Command, Debug, Parser, Runnable)]
pub enum ClearCmds {
    /// Clear outstanding packets (i.e., packet-recv and packet-ack)
    /// on a given channel in both directions. The channel is identified
    /// by the chain, port, and channel IDs at one of its ends.
    Packets(ClearPacketsCmd),
    /// Clear problem packets manually (i.e. packet-recv and packet-ack)
    /// on a given channel. The packet is identified by its sequence number
    /// and submission height on the src chain. The channel is identified by
    /// the chain, port, and channel ID at one of its ends
    Manual(ClearManualCmd),
}

#[derive(Debug, Parser, Command, PartialEq, Eq)]
pub struct ClearPacketsCmd {
    #[clap(
        long = "chain",
        required = true,
        value_name = "CHAIN_ID",
        help_heading = "REQUIRED",
        help = "Identifier of the chain"
    )]
    chain_id: ChainId,

    #[clap(
        long = "port",
        required = true,
        value_name = "PORT_ID",
        help_heading = "REQUIRED",
        help = "Identifier of the port"
    )]
    port_id: PortId,

    #[clap(
        long = "channel",
        alias = "chan",
        required = true,
        value_name = "CHANNEL_ID",
        help_heading = "REQUIRED",
        help = "Identifier of the channel"
    )]
    channel_id: ChannelId,

    #[clap(
        long = "key-name",
        help = "use the given signing key for the specified chain (default: `key_name` config)"
    )]
    key_name: Option<String>,

    #[clap(
        long = "counterparty-key-name",
        help = "use the given signing key for the counterparty chain (default: `counterparty_key_name` config)"
    )]
    counterparty_key_name: Option<String>,
}

impl Override<Config> for ClearPacketsCmd {
    fn override_config(&self, mut config: Config) -> Result<Config, abscissa_core::FrameworkError> {
        let chain_config = config.find_chain_mut(&self.chain_id).ok_or_else(|| {
            FrameworkErrorKind::ComponentError.context(format!(
                "missing configuration for chain '{}'",
                self.chain_id
            ))
        })?;

        if let Some(ref key_name) = self.key_name {
            chain_config.key_name = key_name.to_string();
        }

        Ok(config)
    }
}

impl Runnable for ClearPacketsCmd {
    fn run(&self) {
        let config = app_config();

        let chains = match spawn_chain_counterparty::<BaseChainHandle>(
            &config,
            &self.chain_id,
            &self.port_id,
            &self.channel_id,
        ) {
            Ok((chains, _)) => chains,
            Err(e) => Output::error(format!("{}", e)).exit(),
        };

        // If `counterparty_key_name` is provided, fetch the counterparty chain's
        // config and overwrite its `key_name` parameter
        if let Some(ref counterparty_key_name) = self.counterparty_key_name {
            match chains.dst.config() {
                Ok(mut dst_chain_cfg) => {
                    dst_chain_cfg.key_name = counterparty_key_name.to_string();
                }
                Err(e) => Output::error(format!("{}", e)).exit(),
            }
        }

        let mut ev_list = vec![];

        // Construct links in both directions.
        let opts = LinkParameters {
            src_port_id: self.port_id.clone(),
            src_channel_id: self.channel_id.clone(),
        };
        let fwd_link = match Link::new_from_opts(chains.src.clone(), chains.dst, opts, false) {
            Ok(link) => link,
            Err(e) => Output::error(format!("{}", e)).exit(),
        };
        let rev_link = match fwd_link.reverse(false) {
            Ok(link) => link,
            Err(e) => Output::error(format!("{}", e)).exit(),
        };

        // Schedule RecvPacket messages for pending packets in both directions.
        // This may produce pending acks which will be processed in the next phase.
        run_and_collect_events(&mut ev_list, || {
            fwd_link.relay_recv_packet_and_timeout_messages()
        });
        run_and_collect_events(&mut ev_list, || {
            rev_link.relay_recv_packet_and_timeout_messages()
        });

        // Schedule AckPacket messages in both directions.
        run_and_collect_events(&mut ev_list, || fwd_link.relay_ack_packet_messages());
        run_and_collect_events(&mut ev_list, || rev_link.relay_ack_packet_messages());

        Output::success(ev_list).exit()
    }
}

fn run_and_collect_events<F>(ev_list: &mut Vec<IbcEvent>, f: F)
where
    F: FnOnce() -> Result<Vec<IbcEvent>, LinkError>,
{
    match f() {
        Ok(mut ev) => ev_list.append(&mut ev),
        Err(e) => Output::error(Error::link(e)).exit(),
    };
}

#[derive(Debug, Parser, Command, PartialEq, Eq)]
pub struct ClearManualCmd {
    #[clap(
        long = "chain",
        required = true,
        value_name = "CHAIN_ID",
        help_heading = "REQUIRED",
        help = "Identifier of the chain"
    )]
    chain_id: ChainId,

    #[clap(
        long = "port",
        required = true,
        value_name = "PORT_ID",
        help_heading = "REQUIRED",
        help = "Identifier of the port"
    )]
    port_id: PortId,

    #[clap(
        long = "channel",
        alias = "chan",
        required = true,
        value_name = "CHANNEL_ID",
        help_heading = "REQUIRED",
        help = "Identifier of the channel"
    )]
    channel_id: ChannelId,

    #[clap(
        long = "sequence",
        alias = "seq",
        required = true,
        value_name = "PACKET_SEQUENCE",
        help_heading = "REQUIRED",
        help = "Sequnce number of the problem packet"
    )]
    packet_sequence: Sequence,

    #[clap(
        long = "tx_hash",
        alias = "hash",
        required = true,
        value_name = "TX_HASH",
        help_heading = "REQUIRED",
        help = "The exact block the problem packet was included in"
    )]
    tx_hash: String,

    #[clap(
        long = "key-name",
        help = "use the given signing key for the specified chain (default: `key_name` config)"
    )]
    key_name: Option<String>,

    #[clap(
        long = "counterparty-key-name",
        help = "use the given signing key for the counterparty chain (default: `counterparty_key_name` config)"
    )]
    counterparty_key_name: Option<String>,
}

impl Override<Config> for ClearManualCmd {
    fn override_config(&self, mut config: Config) -> Result<Config, abscissa_core::FrameworkError> {
        let chain_config = config.find_chain_mut(&self.chain_id).ok_or_else(|| {
            FrameworkErrorKind::ComponentError.context(format!(
                "missing configuration for chain '{}'",
                self.chain_id
            ))
        })?;

        if let Some(ref key_name) = self.key_name {
            chain_config.key_name = key_name.to_string();
        }

        Ok(config)
    }
}

impl Runnable for ClearManualCmd {
    fn run(&self) {
        let config = app_config();

        let chains = match spawn_chain_counterparty::<BaseChainHandle>(
            &config,
            &self.chain_id,
            &self.port_id,
            &self.channel_id,
        ) {
            Ok((chains, _)) => chains,
            Err(e) => Output::error(format!("{}", e)).exit(),
        };
        let rt = TokioRuntime::new().unwrap();

        // If `counterparty_key_name` is provided, fetch the counterparty chain's
        // config and overwrite its `key_name` parameter
        if let Some(ref counterparty_key_name) = self.counterparty_key_name {
            match chains.dst.config() {
                Ok(mut dst_chain_cfg) => {
                    dst_chain_cfg.key_name = counterparty_key_name.to_string();
                }
                Err(e) => Output::error(format!("{}", e)).exit(),
            }
        }

        // Construct links in both directions.
        let opts = LinkParameters {
            src_port_id: self.port_id.clone(),
            src_channel_id: self.channel_id.clone(),
        };
        let fwd_link = match Link::new_from_opts(chains.src.clone(), chains.dst, opts, false) {
            Ok(link) => link,
            Err(e) => Output::error(format!("{}", e)).exit(),
        };
        let rev_link = match fwd_link.reverse(false) {
            Ok(link) => link,
            Err(e) => Output::error(format!("{}", e)).exit(),
        };
        // Schedule RecvPacket messages for pending packets in both directions.
        // This may produce pending acks which will be processed in the next phase.
        let src_grpc_addr = chains
            .src
            .config()
            .expect("No src config!")
            .grpc_addr
            .to_string();

        let (_get_tx_response, events, event_height) =
            do_async_stuff(rt.handle(), self.tx_hash.clone(), src_grpc_addr);

        let mut attributes: Option<HashMap<String, String>> = None;
        for ev in events {
            if ev.r#type == SEND_PACKET_EVENT_KEY {
                println!("Found a send packet event: {:?}", ev);
                let mut attrs = HashMap::new();
                let _ = ev
                    .attributes
                    .into_iter()
                    .map(|att| {
                        let k_bytes = att.key;
                        let v_bytes = att.value;
                        let key = std::str::from_utf8(&k_bytes).expect("Non-UTF8 Key!");
                        let val = std::str::from_utf8(&v_bytes).expect("Non-UTF8 Value!");
                        attrs.insert(key.to_string(), val.to_string())
                    })
                    .collect::<Vec<_>>();
                attributes = Some(attrs);
            }
        }

        let attributes = attributes.expect("Could not find a send_packet event on the given tx!");
        let ibc_event: IbcEvent = {
            let sequence = Sequence::from(
                attributes
                    .get("packet_sequence")
                    .expect("No packet_sequence attribute!")
                    .parse::<u64>()
                    .expect("Invalid packet_sequence!"),
            );
            let source_port = PortId::from_str(
                attributes
                    .get("packet_src_port")
                    .expect("No packet_src_port!"),
            )
            .expect("Invalid packet_src_port!");
            let source_channel = ChannelId::from_str(
                attributes
                    .get("packet_src_channel")
                    .expect("No packet_src_channel!"),
            )
            .expect("Invalid packet_src_channel!");
            let destination_port = PortId::from_str(
                attributes
                    .get("packet_dst_port")
                    .expect("No packet_dst_port!"),
            )
            .expect("Invalid packet_dst_port!");
            let destination_channel = ChannelId::from_str(
                attributes
                    .get("packet_dst_channel")
                    .expect("No packet_dst_channel!"),
            )
            .expect("Invalid packet_dst_channel!");
            let data = attributes
                .get("packet_data")
                .expect("No packet_data!")
                .to_string()
                .as_bytes()
                .to_vec();
            let timeout_height_str = attributes
                .get("packet_timeout_height")
                .expect("No packet_timeout_height!")
                .to_string();
            let timeout_height_parts: Vec<&str> = timeout_height_str.split('-').collect();
            assert_eq!(
                timeout_height_parts.len(),
                2,
                "invalid packet_timeout_height, expected 2 values separated by '-'!"
            );
            let revision_number: u64 = timeout_height_parts
                .get(0)
                .unwrap()
                .to_string()
                .parse()
                .expect("Invalid revision number in timeout height!");
            println!(
                "Timeout height parts: {:?}, original str {:?}",
                timeout_height_parts, timeout_height_str
            );
            let revision_height: u64 = timeout_height_parts
                .get(1)
                .unwrap()
                .to_string()
                .parse()
                .expect("Invalid revision height in timeout height!");
            let timeout_height = Channel::timeout::TimeoutHeight::At(
                IbcHeight::new(revision_number, revision_height)
                    .expect("Could not create valid IBCHeight"),
            );
            let timeout_timestamp = ibc::timestamp::Timestamp::from_str(
                attributes
                    .get("packet_timeout_timestamp")
                    .expect("No packet_timeout_timestamp!"),
            )
            .unwrap();

            IbcEvent::SendPacket(ChannelEvents::SendPacket {
                packet: Channel::packet::Packet {
                    sequence,
                    source_port,
                    source_channel,
                    destination_port,
                    destination_channel,
                    data,
                    timeout_height,
                    timeout_timestamp,
                },
            })
        };
        println!("Constructed base IbcEvent: {:?}", ibc_event);
        let ibc_event = IbcEventWithHeight::new(ibc_event, event_height);
        println!("Constructed IbcEventWithHeight: {:?}", ibc_event);
        let tracking_id = TrackingId::new_static("packet-recv");
        let tracked_event = TrackedEvents::new(vec![ibc_event.clone()], tracking_id.clone());
        println!("Scheduling the packet to be sent using the fwd_link");
        let _fwd_operational_data = fwd_link.a_to_b.events_to_operational_data(tracked_event);

        println!("Attempting to push the packet along fwd_link immediately");
        let mut results = vec![];
        // In case of zero connection delay, the op. data will already be ready
        let (src_ods, dst_ods) = fwd_link
            .a_to_b
            .try_fetch_scheduled_operational_data()
            .expect("try_fetch_scheduled_op_data failed");
        println!(
            "Fetched fwd_link operational data - Src OD: {:?} | Dst OD: {:?}",
            src_ods, dst_ods
        );
        relay_and_accumulate_results(&fwd_link.a_to_b, Vec::from(src_ods), &mut results)
            .expect("relay_and_accumulate failed!");
        relay_and_accumulate_results(&fwd_link.a_to_b, Vec::from(dst_ods), &mut results)
            .expect("relay_and_accumulate failed!");

        // In case of non-zero connection delay, we block here waiting for all op.data
        // until the connection delay elapses
        while let Some(odata) = fwd_link
            .a_to_b
            .fetch_scheduled_operational_data()
            .expect("fetch_scheduled_op_data failed")
        {
            relay_and_accumulate_results(&fwd_link.a_to_b, vec![odata], &mut results)
                .expect("relay_and_accumulate failed");
        }

        println!("Scheduling any remaining work to be sent using the rev_link");
        let tracked_event = TrackedEvents::new(vec![ibc_event.clone()], tracking_id.clone());
        let _rev_operational_data = rev_link.a_to_b.events_to_operational_data(tracked_event);

        println!("Attempting to push the packet along rev_link immediately");
        let mut results = vec![];
        // In case of zero connection delay, the op. data will already be ready
        let (src_ods, dst_ods) = rev_link
            .a_to_b
            .try_fetch_scheduled_operational_data()
            .expect("try_fetch_scheduled_op_data failed");
        println!(
            "Fetched rev_link operational data - Src OD: {:?} | Dst OD: {:?}",
            src_ods, dst_ods
        );
        relay_and_accumulate_results(&rev_link.a_to_b, Vec::from(src_ods), &mut results)
            .expect("try_fetch_scheduled_op_data failed");
        relay_and_accumulate_results(&rev_link.a_to_b, Vec::from(dst_ods), &mut results)
            .expect("try_fetch_scheduled_op_data failed");

        // In case of non-zero connection delay, we block here waiting for all op.data
        // until the connection delay elapses
        while let Some(odata) = rev_link
            .a_to_b
            .fetch_scheduled_operational_data()
            .expect("fetch_scheduled_op_data failed")
        {
            relay_and_accumulate_results(&rev_link.a_to_b, vec![odata], &mut results)
                .expect("relay_and_accumulate failed");
        }

        // // Schedule AckPacket messages in both directions.
        // run_and_collect_events(&mut ev_list, || fwd_link.relay_ack_packet_messages());
        // run_and_collect_events(&mut ev_list, || rev_link.relay_ack_packet_messages());

        // Output::success(ev_list).exit()
    }
}

pub fn relay_and_accumulate_results<ChainA: ChainHandle, ChainB: ChainHandle>(
    path: &RelayPath<ChainA, ChainB>,
    from: Vec<OperationalData>,
    results: &mut Vec<IbcEvent>,
) -> Result<(), LinkError> {
    for od in from {
        let mut last_res = relay_from_operational_data::<SyncSender, ChainA, ChainB>(path, od)?;
        results.append(&mut last_res.events);
    }

    Ok(())
}
pub fn relay_from_operational_data<S: Submit, ChainA: ChainHandle, ChainB: ChainHandle>(
    path: &RelayPath<ChainA, ChainB>,
    initial_od: OperationalData,
) -> Result<S::Reply, LinkError> {
    // We will operate on potentially different operational data if the initial one fails.
    let _span = span!(Level::INFO, "relay", odata = %initial_od.info()).entered();

    let mut odata = initial_od;

    for i in 0..5 {
        debug!("[try {}/{}]", i + 1, 5);

        // Consume the operational data by attempting to send its messages
        match send_from_operational_data::<S, ChainA, ChainB>(path, &odata) {
            Ok(reply) => {
                // Done with this op. data
                info!("success");
                telemetry!({
                    let (chain, counterparty, channel_id, port_id) = path.target_info(odata.target);

                    ibc_telemetry::global().tx_submitted(
                        reply.len(),
                        odata.tracking_id,
                        &chain,
                        channel_id,
                        port_id,
                        &counterparty,
                    );
                });

                return Ok(reply);
            }
            Err(LinkError(link::error::LinkErrorDetail::Send(e), _)) => {
                // This error means we could retry
                error!("error {}", e.event);
                if i + 1 == 5 {
                    error!("{}/{} retries exhausted. giving up", i + 1, 5)
                } else {
                    // If we haven't exhausted all retries, regenerate the op. data & retry
                    match path.regenerate_operational_data(odata.clone()) {
                        None => return Ok(S::Reply::empty()), // Nothing to retry
                        Some(new_od) => odata = new_od,
                    }
                }
            }
            Err(e) => {
                // Unrecoverable error, propagate up the stack
                return Err(e);
            }
        }
    }

    Ok(S::Reply::empty())
}

pub fn send_from_operational_data<S: Submit, ChainA: ChainHandle, ChainB: ChainHandle>(
    path: &RelayPath<ChainA, ChainB>,
    odata: &OperationalData,
) -> Result<S::Reply, LinkError> {
    if odata.batch.is_empty() {
        error!("ignoring empty operational data!");
        return Ok(S::Reply::empty());
    }

    // The following line fails to generate an update msg, we only want the MsgPacketRecv
    // let msgs = odata.assemble_msgs(self)?;
    // Replace the above with the following:
    // REPLACEMENT START
    let msgs: Vec<Any> = odata.batch.iter().map(|gm| gm.msg.clone()).collect();

    let tm = TrackedMsgs::new(msgs, odata.tracking_id);

    info!("assembled batch of {} message(s)", tm.messages().len());
    // REPLACEMENT END

    match odata.target {
        OperationalDataTarget::Source => S::submit(path.src_chain(), tm.clone()),
        OperationalDataTarget::Destination => S::submit(path.dst_chain(), tm.clone()),
    }
}

pub fn target_info<ChainA: ChainHandle, ChainB: ChainHandle>(
    path: &RelayPath<ChainA, ChainB>,
    target: OperationalDataTarget,
) -> (
    ChainId, // source chain
    ChainId, // destination chain
    &ChannelId,
    &PortId,
) {
    match target {
        OperationalDataTarget::Source => (
            path.src_chain().id(),
            path.dst_chain().id(),
            path.src_channel_id(),
            path.src_port_id(),
        ),
        OperationalDataTarget::Destination => (
            path.dst_chain().id(),
            path.src_chain().id(),
            path.dst_channel_id(),
            path.dst_port_id(),
        ),
    }
}

#[derive(Debug, Clone)]
struct AsyncStuff {
    pub get_tx_res: GetTxResponse,
    pub events: Vec<TmEvent>,
    pub event_height: IbcHeight,
}

fn do_async_stuff(
    handle: &tokio::runtime::Handle,
    tx_hash: String,
    src_grpc_addr: String,
) -> (GetTxResponse, Vec<TmEvent>, IbcHeight) {
    // let (sendr, recvr) = unbounded();

    let res = handle.block_on(async move {
        // do something async
        // });
        // let res = tokio::task::block_in_place(async {
        let mut txsrv = TxServiceClient::connect(src_grpc_addr.clone())
            .await
            .expect("Unable to contact src chain via grpc");

        let mut tmsrv = TmServiceClient::connect(src_grpc_addr.clone())
            .await
            .expect("Unable to contact src chain via grpc");

        let get_tx_res = txsrv
            .get_tx(GetTxRequest {
                hash: tx_hash.clone(),
            })
            .await
            .expect("failed to get tx by hash!")
            .into_inner();
        let events = get_tx_res
            .tx_response
            .clone()
            .expect(&format!(
                "No tx response found with hash {}",
                tx_hash.clone()
            ))
            .events;

        for event in &events {
            println!("Discovered event: {:?}", event);
        }

        let src_latest_header = tmsrv
            .get_latest_block(GetLatestBlockRequest {})
            .await
            .expect("Unable to get latest block")
            .into_inner()
            .block
            .expect("Latest block not found!")
            .header
            .expect("Latest block has no header!");

        // Get the revision number for the chain from the chain id
        let chain_id = src_latest_header.chain_id.clone();
        let revision_number: u64 = chain_id
            .split('-')
            .last()
            .expect("Chain id has no -'s in it!")
            .parse()
            .expect("Invalid revision number at end of chain id!");

        let latest_height = src_latest_header.height.clone();
        // Overwrite the latest height with one known to Canto right now
        let latest_height = 3650065;
        let latest_height =
            IbcHeight::new(revision_number, latest_height as u64).expect("Invalid IbcHeight made!");

        // let event_height = IbcHeight::new(
        //     revision_number,
        //     get_tx_res.tx_response.clone().unwrap().height as u64,
        // )
        // .expect("Invalid IbcHeight made");

        return AsyncStuff {
            get_tx_res,
            events,
            // event_height,
            event_height: latest_height,
        };
        // drop(sendr);
        // sendr.send((get_tx_res, events, event_height))
    });

    // let res = recvr.recv().expect("Unable to recv!");

    let stuff = res;
    (stuff.get_tx_res, stuff.events, stuff.event_height)
}

#[cfg(test)]
mod tests {
    use super::ClearPacketsCmd;

    use std::str::FromStr;

    use abscissa_core::clap::Parser;
    use ibc::core::ics24_host::identifier::{ChainId, ChannelId, PortId};

    #[test]
    fn test_clear_packets_required_only() {
        assert_eq!(
            ClearPacketsCmd {
                chain_id: ChainId::from_string("chain_id"),
                port_id: PortId::from_str("port_id").unwrap(),
                channel_id: ChannelId::from_str("channel-07").unwrap(),
                key_name: None,
                counterparty_key_name: None,
            },
            ClearPacketsCmd::parse_from(&[
                "test",
                "--chain",
                "chain_id",
                "--port",
                "port_id",
                "--channel",
                "channel-07"
            ])
        )
    }

    #[test]
    fn test_clear_packets_chan_alias() {
        assert_eq!(
            ClearPacketsCmd {
                chain_id: ChainId::from_string("chain_id"),
                port_id: PortId::from_str("port_id").unwrap(),
                channel_id: ChannelId::from_str("channel-07").unwrap(),
                key_name: None,
                counterparty_key_name: None
            },
            ClearPacketsCmd::parse_from(&[
                "test",
                "--chain",
                "chain_id",
                "--port",
                "port_id",
                "--chan",
                "channel-07"
            ])
        )
    }

    #[test]
    fn test_clear_packets_key_name() {
        assert_eq!(
            ClearPacketsCmd {
                chain_id: ChainId::from_string("chain_id"),
                port_id: PortId::from_str("port_id").unwrap(),
                channel_id: ChannelId::from_str("channel-07").unwrap(),
                key_name: Some("key_name".to_owned()),
                counterparty_key_name: None,
            },
            ClearPacketsCmd::parse_from(&[
                "test",
                "--chain",
                "chain_id",
                "--port",
                "port_id",
                "--channel",
                "channel-07",
                "--key-name",
                "key_name"
            ])
        )
    }

    #[test]
    fn test_clear_packets_counterparty_key_name() {
        assert_eq!(
            ClearPacketsCmd {
                chain_id: ChainId::from_string("chain_id"),
                port_id: PortId::from_str("port_id").unwrap(),
                channel_id: ChannelId::from_str("channel-07").unwrap(),
                key_name: None,
                counterparty_key_name: Some("counterparty_key_name".to_owned()),
            },
            ClearPacketsCmd::parse_from(&[
                "test",
                "--chain",
                "chain_id",
                "--port",
                "port_id",
                "--channel",
                "channel-07",
                "--counterparty-key-name",
                "counterparty_key_name"
            ])
        )
    }

    #[test]
    fn test_clear_packets_no_chan() {
        assert!(ClearPacketsCmd::try_parse_from(&[
            "test", "--chain", "chain_id", "--port", "port_id"
        ])
        .is_err())
    }

    #[test]
    fn test_clear_packets_no_port() {
        assert!(ClearPacketsCmd::try_parse_from(&[
            "test",
            "--chain",
            "chain_id",
            "--channel",
            "channel-07"
        ])
        .is_err())
    }

    #[test]
    fn test_clear_packets_no_chain() {
        assert!(ClearPacketsCmd::try_parse_from(&[
            "test",
            "--port",
            "port_id",
            "--channel",
            "channel-07"
        ])
        .is_err())
    }
}
