//! MPEG-4 DST 区间算术解码器 (Arithmetic Decoder)
//!
//! 纯 Rust 移植自 ISO/IEC 14496-3 DST 算术解码规范。

pub const AC_BITS: usize = 8;
pub const AC_PROBS: usize = 1 << AC_BITS; // 256

/// 算术解码器状态机
pub struct ArithmeticDecoder<'a> {
    data: &'a [u8],
    byte_pos: usize,
    a: u32,
    c: u32,
}

impl<'a> ArithmeticDecoder<'a> {
    /// 初始化算术解码器并装载初始状态
    pub fn new(data: &'a [u8]) -> Self {
        let mut dec = Self {
            data,
            byte_pos: 0,
            a: 0,
            c: 0,
        };

        // 预加载前两个字节
        let b0 = dec.read_byte() as u32;
        let b1 = dec.read_byte() as u32;
        dec.c = (b0 << 8) | b1;
        dec.a = 0xFFFF;

        dec
    }

    #[inline(always)]
    fn read_byte(&mut self) -> u8 {
        if self.byte_pos < self.data.len() {
            let b = self.data[self.byte_pos];
            self.byte_pos += 1;
            b
        } else {
            0
        }
    }

    /// 根据当前上下文概率 p 解码一个比特
    /// p: 符号为 1 的概率 (0..=256)
    #[inline(always)]
    pub fn decode_bit(&mut self, p: u32) -> u8 {
        let a_bound = (self.a * p) >> AC_BITS;

        let bit = if self.c < a_bound {
            self.a = a_bound;
            1
        } else {
            self.c -= a_bound;
            self.a -= a_bound;
            0
        };

        // 归一化 (Renormalization)
        while self.a < 0x8000 {
            self.a <<= 1;
            self.c = (self.c << 1) | (self.read_next_bit() as u32);
        }

        bit
    }

    #[inline(always)]
    fn read_next_bit(&mut self) -> u8 {
        let b = self.read_byte();
        (b >> 7) & 1
    }
}
