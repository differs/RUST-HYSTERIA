use crate::protocol::UDPMessage;

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
    packet_id: u16,
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

        if message.packet_id != self.packet_id
            || message.frag_count as usize != self.fragments.len()
        {
            self.packet_id = message.packet_id;
            self.fragments = vec![None; message.frag_count as usize];
            self.fragments[message.frag_id as usize] = Some(message.clone());
            self.count = 1;
            self.size = message.data.len();
            return None;
        }

        if self.fragments[message.frag_id as usize].is_none() {
            self.size += message.data.len();
            self.count += 1;
            self.fragments[message.frag_id as usize] = Some(message.clone());
            if self.count as usize == self.fragments.len() {
                let mut assembled = message;
                let mut data = Vec::with_capacity(self.size);
                for fragment in &self.fragments {
                    data.extend_from_slice(&fragment.as_ref().expect("fragment must exist").data);
                }
                assembled.data = data;
                assembled.frag_id = 0;
                assembled.frag_count = 1;
                return Some(assembled);
            }
        }

        None
    }
}
