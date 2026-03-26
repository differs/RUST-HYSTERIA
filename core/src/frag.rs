use std::collections::{HashMap, VecDeque};

use crate::protocol::UDPMessage;

const MAX_IN_FLIGHT_PACKETS: usize = 64;

pub fn frag_udp_message(message: &UDPMessage, max_size: usize) -> Vec<UDPMessage> {
    if message.size() <= max_size {
        return vec![message.clone()];
    }

    let full_payload = &message.data;
    let max_payload_size = max_size - message.header_size();
    let frag_count = full_payload.len().div_ceil(max_payload_size) as u8;
    let mut fragments = Vec::with_capacity(frag_count as usize);
    let mut offset = 0;
    let mut frag_id = 0_u8;

    while offset < full_payload.len() {
        let payload_size = (full_payload.len() - offset).min(max_payload_size);
        let mut fragment = message.clone();
        fragment.frag_id = frag_id;
        fragment.frag_count = frag_count;
        fragment.data = full_payload[offset..offset + payload_size].to_vec();
        fragments.push(fragment);
        offset += payload_size;
        frag_id += 1;
    }

    fragments
}

#[derive(Debug, Default, Clone)]
pub struct Defragger {
    packets: HashMap<u16, PacketState>,
    order: VecDeque<u16>,
}

#[derive(Debug, Clone)]
struct PacketState {
    session_id: u32,
    addr: String,
    fragments: Vec<Option<UDPMessage>>,
    count: u8,
    size: usize,
}

impl Defragger {
    pub fn feed(&mut self, message: UDPMessage) -> Option<UDPMessage> {
        if message.frag_count <= 1 {
            return Some(message);
        }
        if message.frag_id >= message.frag_count {
            return None;
        }

        let packet_id = message.packet_id;
        let frag_index = message.frag_id as usize;
        let frag_count = message.frag_count as usize;

        let reset_packet = self
            .packets
            .get(&packet_id)
            .map(|state| {
                state.session_id != message.session_id
                    || state.addr != message.addr
                    || state.fragments.len() != frag_count
            })
            .unwrap_or(true);

        if reset_packet {
            self.insert_packet(packet_id, frag_count, &message);
            return None;
        }

        if let Some(state) = self.packets.get_mut(&packet_id) {
            if state.fragments[frag_index].is_none() {
                state.size += message.data.len();
                state.count += 1;
                state.fragments[frag_index] = Some(message.clone());
            }
            if state.count as usize == state.fragments.len() {
                let mut assembled = message;
                let mut data = Vec::with_capacity(state.size);
                for fragment in &state.fragments {
                    data.extend_from_slice(&fragment.as_ref().expect("fragment must exist").data);
                }
                assembled.data = data;
                assembled.frag_id = 0;
                assembled.frag_count = 1;
                self.remove_packet(packet_id);
                return Some(assembled);
            }
        }

        None
    }

    fn insert_packet(&mut self, packet_id: u16, frag_count: usize, message: &UDPMessage) {
        self.remove_packet(packet_id);
        while self.order.len() >= MAX_IN_FLIGHT_PACKETS {
            if let Some(oldest) = self.order.pop_front() {
                self.packets.remove(&oldest);
            }
        }

        let mut fragments = vec![None; frag_count];
        fragments[message.frag_id as usize] = Some(message.clone());
        self.packets.insert(
            packet_id,
            PacketState {
                session_id: message.session_id,
                addr: message.addr.clone(),
                fragments,
                count: 1,
                size: message.data.len(),
            },
        );
        self.order.push_back(packet_id);
    }

    fn remove_packet(&mut self, packet_id: u16) {
        self.packets.remove(&packet_id);
        if let Some(index) = self.order.iter().position(|id| *id == packet_id) {
            self.order.remove(index);
        }
    }
}
