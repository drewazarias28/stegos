//! mod.rs - general utility functions for vector handling

//
// Copyright (c) 2018 Stegos AG
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use crate::CryptoError;

use hex;
use std::cmp::Ordering;
use std::fmt::Write;
// -------------------------------------------------------------------
// general utility functions

pub fn hexstr_to_bev_u8(s: &str, x: &mut [u8]) -> Result<(), CryptoError> {
    // collect a big-endian vector of 8-bit values from a hex string.
    let mut sx = String::from(s);
    if (s.len() & 1) != 0 {
        // we have an odd number of hex digits.
        sx.insert(0, '0');
    }
    let v = hex::decode(sx)?;
    let nel = x.len();
    if nel < v.len() {
        return Err(CryptoError::InvalidHexLength);
    }
    let mut ix = 0; // this seems dumb... isn't there a better way?
    for b in v {
        x[ix] = b;
        ix += 1;
    }
    while ix < nel {
        x[ix] = 0;
        ix += 1;
    }
    Ok(())
}

pub fn hexstr_to_lev_u8(s: &str, x: &mut [u8]) -> Result<bool, CryptoError> {
    // collect a little-endian vector of 8-bit values from a hex string.
    let mut sx = String::from(s);
    if (s.len() & 1) != 0 {
        // we have an odd number of hex digits.
        sx.insert(0, '0');
    }
    let v = hex::decode(sx)?;
    let nel = x.len();
    if nel < v.len() {
        return Err(CryptoError::InvalidHexLength);
    }

    let mut ix = nel;
    // allow for shorter answer than room allotted for it.
    // zero pad the MSB's
    for _ in v.len()..nel {
        ix -= 1;
        x[ix] = 0;
    }
    for b in v {
        ix -= 1;
        x[ix] = b;
    }
    Ok(true)
}

pub fn u8v_to_hexstr(x: &[u8]) -> String {
    // produce a hexnum string from a byte vector
    let mut s = String::new();
    for ix in 0..x.len() {
        s.push_str(&format!("{:02x}", x[ix]));
    }
    s
}

/// Print first nbits of data.
/// Format of output is "[{bits}]", where {bits} is digits sequence (0 or 1).
pub fn print_nbits(data: &[u8], nbits: usize) -> Result<String, std::fmt::Error> {
    let mut result = String::new();
    write!(&mut result, "[")?;
    for i in 0..nbits {
        let byte = i / 8;
        let bit = i % 8;
        let num = if 0 != (data[byte] & (1 << bit)) { 1 } else { 0 };
        write!(&mut result, "{}", num)?;
    }

    write!(&mut result, "]")?;
    Ok(result)
}

pub fn is_zero_bits(v: &[u8]) -> bool {
    v.iter().fold(false, |_, b| {
        if *b != 0 {
            return false;
        }
        true
    })
}

pub fn is_one_bits(v: &[u8]) -> bool {
    v.iter().fold(false, |_, b| {
        if *b != 0xff {
            return false;
        }
        true
    })
}

pub fn ucmp_be(a: &[u8], b: &[u8]) -> Ordering {
    for (xa, xb) in a.iter().zip(b.iter()) {
        if *xa < *xb {
            return Ordering::Less;
        } else if *xa > *xb {
            return Ordering::Greater;
        }
    }
    Ordering::Equal
}

pub fn ucmp_le(a: &[u8], b: &[u8]) -> Ordering {
    for (xa, xb) in a.iter().zip(b.iter()).rev() {
        if *xa < *xb {
            return Ordering::Less;
        } else if *xa > *xb {
            return Ordering::Greater;
        }
    }
    Ordering::Equal
}

pub fn ushr_be(src: &[u8], dst: &mut [u8], nsh: usize) {
    let len = src.len();
    assert!(len == dst.len());
    let nb = {
        let nb = nsh >> 3;
        if nb >= len {
            len
        } else {
            nb
        }
    };
    let nbits = nsh & 7;
    let lsh = 8 - nbits;
    for elt in dst[0..nb].iter_mut() {
        *elt = 0;
    }
    let mut tmp = 0;
    for (elt, x) in dst[nb..len].iter_mut().zip(src[0..(len - nb)].iter()) {
        *elt = tmp | (*x >> nbits);
        tmp = *x << lsh;
    }
}

pub fn ushr_le(src: &[u8], dst: &mut [u8], nsh: usize) {
    let len = src.len();
    assert!(len == dst.len());
    let nb = {
        let nb = nsh >> 3;
        if nb >= len {
            len
        } else {
            nb
        }
    };
    let nbits = nsh & 7;
    let lsh = 8 - nbits;
    for elt in dst[(len - nb)..len].iter_mut() {
        *elt = 0;
    }
    let mut tmp = 0;
    for (elt, x) in dst[0..(len - nb)].iter_mut().zip(src[nb..len].iter()).rev() {
        *elt = tmp | (*x >> nbits);
        tmp = *x << lsh;
    }
}
