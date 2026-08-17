// SPDX-License-Identifier: Apache-2.0

// Minimal safe, unkeyed BLAKE3 and CRC32C primitives keep the proof format independent of
// transitive dependency visibility. Golden tests cover the standard empty and `abc` vectors.

const IV: [u32; 8] = [
    0x6A09_E667,
    0xBB67_AE85,
    0x3C6E_F372,
    0xA54F_F53A,
    0x510E_527F,
    0x9B05_688C,
    0x1F83_D9AB,
    0x5BE0_CD19,
];
const CHUNK_START: u32 = 1;
const CHUNK_END: u32 = 2;
const PARENT: u32 = 4;
const ROOT: u32 = 8;
const CHUNK_BYTES: usize = 1_024;
const BLOCK_BYTES: usize = 64;
const PERMUTATION: [usize; 16] = [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8];

#[derive(Clone, Copy)]
struct Output {
    input_cv: [u32; 8],
    block: [u32; 16],
    counter: u64,
    block_len: u32,
    flags: u32,
}

impl Output {
    fn chaining_value(self) -> [u32; 8] {
        let words = compress(
            self.input_cv,
            self.block,
            self.counter,
            self.block_len,
            self.flags,
        );
        let mut result = [0_u32; 8];
        result.copy_from_slice(&words[..8]);
        result
    }

    fn root_hash(self) -> [u8; 32] {
        let words = compress(
            self.input_cv,
            self.block,
            0,
            self.block_len,
            self.flags | ROOT,
        );
        let mut result = [0_u8; 32];
        for (destination, word) in result.chunks_exact_mut(4).zip(words) {
            destination.copy_from_slice(&word.to_le_bytes());
        }
        result
    }
}

pub(super) fn blake3(input: &[u8]) -> [u8; 32] {
    let mut hasher = Blake3Hasher::default();
    hasher.update(input);
    hasher.finalize()
}

pub(super) fn blake3_parts(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Blake3Hasher::default();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize()
}

struct Blake3Hasher {
    chunk: [u8; CHUNK_BYTES],
    chunk_len: usize,
    completed_chunks: u64,
    cv_stack: [[u32; 8]; 64],
    stack_len: usize,
}

impl Default for Blake3Hasher {
    fn default() -> Self {
        Self {
            chunk: [0; CHUNK_BYTES],
            chunk_len: 0,
            completed_chunks: 0,
            cv_stack: [[0; 8]; 64],
            stack_len: 0,
        }
    }
}

impl Blake3Hasher {
    fn update(&mut self, mut input: &[u8]) {
        while !input.is_empty() {
            if self.chunk_len == CHUNK_BYTES {
                self.push_completed_chunk();
            }
            let available = CHUNK_BYTES - self.chunk_len;
            let take = available.min(input.len());
            self.chunk[self.chunk_len..self.chunk_len + take].copy_from_slice(&input[..take]);
            self.chunk_len += take;
            input = &input[take..];
        }
    }

    fn push_completed_chunk(&mut self) {
        let output = chunk_output(&self.chunk, self.completed_chunks);
        self.completed_chunks = self.completed_chunks.saturating_add(1);
        add_chunk_cv(
            output.chaining_value(),
            self.completed_chunks,
            &mut self.cv_stack,
            &mut self.stack_len,
        );
        self.chunk_len = 0;
    }

    fn finalize(mut self) -> [u8; 32] {
        let mut output = chunk_output(&self.chunk[..self.chunk_len], self.completed_chunks);
        while self.stack_len > 0 {
            self.stack_len -= 1;
            output = parent_output(self.cv_stack[self.stack_len], output.chaining_value());
        }
        output.root_hash()
    }
}

fn add_chunk_cv(
    mut new_cv: [u32; 8],
    mut total_chunks: u64,
    stack: &mut [[u32; 8]; 64],
    stack_len: &mut usize,
) {
    while total_chunks & 1 == 0 {
        if *stack_len > 0 {
            *stack_len -= 1;
            new_cv = parent_output(stack[*stack_len], new_cv).chaining_value();
        }
        total_chunks >>= 1;
    }
    if *stack_len < stack.len() {
        stack[*stack_len] = new_cv;
        *stack_len += 1;
    }
}

fn chunk_output(chunk: &[u8], counter: u64) -> Output {
    let block_count = chunk.len().max(1).div_ceil(BLOCK_BYTES);
    let mut cv = IV;
    for block_index in 0..block_count.saturating_sub(1) {
        let start = block_index * BLOCK_BYTES;
        let block = block_words(&chunk[start..start + BLOCK_BYTES]);
        let flags = if block_index == 0 { CHUNK_START } else { 0 };
        let words = compress(cv, block, counter, 64, flags);
        cv.copy_from_slice(&words[..8]);
    }
    let final_index = block_count - 1;
    let final_start = final_index * BLOCK_BYTES;
    let final_bytes = &chunk[final_start..];
    Output {
        input_cv: cv,
        block: block_words(final_bytes),
        counter,
        block_len: u32::try_from(final_bytes.len()).unwrap_or(u32::MAX),
        flags: CHUNK_END | if final_index == 0 { CHUNK_START } else { 0 },
    }
}

fn parent_output(left: [u32; 8], right: [u32; 8]) -> Output {
    let mut block = [0_u32; 16];
    block[..8].copy_from_slice(&left);
    block[8..].copy_from_slice(&right);
    Output {
        input_cv: IV,
        block,
        counter: 0,
        block_len: 64,
        flags: PARENT,
    }
}

fn block_words(bytes: &[u8]) -> [u32; 16] {
    let mut block = [0_u8; BLOCK_BYTES];
    block[..bytes.len()].copy_from_slice(bytes);
    let mut words = [0_u32; 16];
    for (word, bytes) in words.iter_mut().zip(block.chunks_exact(4)) {
        *word = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    }
    words
}

fn compress(
    chaining_value: [u32; 8],
    block: [u32; 16],
    counter: u64,
    block_len: u32,
    flags: u32,
) -> [u32; 16] {
    let counter_bytes = counter.to_le_bytes();
    let counter_low = u32::from_le_bytes([
        counter_bytes[0],
        counter_bytes[1],
        counter_bytes[2],
        counter_bytes[3],
    ]);
    let counter_high = u32::from_le_bytes([
        counter_bytes[4],
        counter_bytes[5],
        counter_bytes[6],
        counter_bytes[7],
    ]);
    let mut state = [0_u32; 16];
    state[..8].copy_from_slice(&chaining_value);
    state[8..12].copy_from_slice(&IV[..4]);
    state[12] = counter_low;
    state[13] = counter_high;
    state[14] = block_len;
    state[15] = flags;
    let mut message = block;
    for _ in 0..7 {
        round(&mut state, message);
        let previous = message;
        for (index, source) in PERMUTATION.into_iter().enumerate() {
            message[index] = previous[source];
        }
    }
    for index in 0..8 {
        state[index] ^= state[index + 8];
        state[index + 8] ^= chaining_value[index];
    }
    state
}

fn round(state: &mut [u32; 16], message: [u32; 16]) {
    g(state, 0, 4, 8, 12, message[0], message[1]);
    g(state, 1, 5, 9, 13, message[2], message[3]);
    g(state, 2, 6, 10, 14, message[4], message[5]);
    g(state, 3, 7, 11, 15, message[6], message[7]);
    g(state, 0, 5, 10, 15, message[8], message[9]);
    g(state, 1, 6, 11, 12, message[10], message[11]);
    g(state, 2, 7, 8, 13, message[12], message[13]);
    g(state, 3, 4, 9, 14, message[14], message[15]);
}

fn g(
    state: &mut [u32; 16],
    a: usize,
    b: usize,
    c: usize,
    d: usize,
    message_x: u32,
    message_y: u32,
) {
    state[a] = state[a].wrapping_add(state[b]).wrapping_add(message_x);
    state[d] = (state[d] ^ state[a]).rotate_right(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_right(12);
    state[a] = state[a].wrapping_add(state[b]).wrapping_add(message_y);
    state[d] = (state[d] ^ state[a]).rotate_right(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_right(7);
}

pub(super) fn crc32c_parts(parts: &[&[u8]]) -> u32 {
    let mut crc = u32::MAX;
    for part in parts {
        for byte in *part {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0x82F6_3B78 & 0_u32.wrapping_sub(crc & 1));
            }
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::{blake3, crc32c_parts};

    #[test]
    fn primitives_match_published_vectors() {
        assert_eq!(
            hex(blake3(b"")),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        assert_eq!(
            hex(blake3(b"abc")),
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
        assert_eq!(crc32c_parts(&[b"123456789"]), 0xE306_9283);
    }

    #[test]
    fn blake3_matches_vectors_across_chunk_tree_boundaries() {
        let bytes = (0_u16..4_096)
            .map(|index| (index % 251).to_le_bytes()[0])
            .collect::<Vec<_>>();
        for (length, expected) in [
            (
                1_023,
                "10108970eeda3eb932baac1428c7a2163b0e924c9a9e25b35bba72b28f70bd11",
            ),
            (
                1_024,
                "42214739f095a406f3fc83deb889744ac00df831c10daa55189b5d121c855af7",
            ),
            (
                1_025,
                "d00278ae47eb27b34faecf67b4fe263f82d5412916c1ffd97c8cb7fb814b8444",
            ),
            (
                2_048,
                "e776b6028c7cd22a4d0ba182a8bf62205d2ef576467e838ed6f2529b85fba24a",
            ),
            (
                4_096,
                "015094013f57a5277b59d8475c0501042c0b642e531b0a1c8f58d2163229e969",
            ),
        ] {
            assert_eq!(hex(blake3(&bytes[..length])), expected);
        }
    }

    fn hex(bytes: [u8; 32]) -> String {
        use std::fmt::Write as _;

        bytes.iter().fold(String::new(), |mut encoded, byte| {
            let _ignored = write!(encoded, "{byte:02x}");
            encoded
        })
    }
}
