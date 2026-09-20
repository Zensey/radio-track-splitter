//! Minimal reader for PyTorch's legacy (pre-1.6, non-zip) `torch.save` format,
//! which candle cannot load. torchvggish's published weights use it.
//!
//! Layout: three header pickles (magic, protocol version, sys info), the state
//! dict pickle (tensors reference storages by persistent id), a pickle listing
//! the storage keys, then for each key an i64 element count and raw data.

use anyhow::{anyhow, bail, Result};
use candle_core::{Device, Tensor};
use std::collections::HashMap;
use std::path::Path;

#[derive(Clone, Debug)]
enum Obj {
    None,
    Int(i64),
    Str(String),
    Tuple(Vec<Obj>),
    List(Vec<Obj>),
    Dict(Vec<(Obj, Obj)>),
    Global(String),
    Mark,
    PersId(Box<Obj>),
    Tensor { key: String, offset: usize, shape: Vec<usize>, stride: Vec<usize> },
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.bytes.len());
        let end = end.ok_or_else(|| anyhow!("unexpected end of weights file"))?;
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into()?))
    }
    fn line(&mut self) -> Result<String> {
        let rest = &self.bytes[self.pos..];
        let end = rest.iter().position(|&b| b == b'\n').ok_or_else(|| anyhow!("unterminated line"))?;
        let text = String::from_utf8_lossy(&rest[..end]).into_owned();
        self.pos += end + 1;
        Ok(text)
    }
    fn string(&mut self, len: usize) -> Result<Obj> {
        Ok(Obj::Str(String::from_utf8(self.take(len)?.to_vec())?))
    }
}

fn pop_to_mark(stack: &mut Vec<Obj>) -> Result<Vec<Obj>> {
    let at = stack
        .iter()
        .rposition(|o| matches!(o, Obj::Mark))
        .ok_or_else(|| anyhow!("pickle MARK missing"))?;
    let items = stack.split_off(at + 1);
    stack.pop();
    Ok(items)
}

fn push_pairs(target: &mut Obj, items: Vec<Obj>) -> Result<()> {
    match target {
        Obj::Dict(pairs) => {
            let mut it = items.into_iter();
            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                pairs.push((k, v));
            }
            Ok(())
        }
        other => bail!("SETITEM on non-dict {other:?}"),
    }
}

fn as_usizes(o: &Obj) -> Result<Vec<usize>> {
    match o {
        Obj::Tuple(items) => items
            .iter()
            .map(|i| match i {
                Obj::Int(v) if *v >= 0 => Ok(*v as usize),
                other => bail!("expected non-negative int, got {other:?}"),
            })
            .collect(),
        other => bail!("expected tuple, got {other:?}"),
    }
}

fn reduce(callable: Obj, args: Obj) -> Result<Obj> {
    let Obj::Global(name) = callable else { bail!("REDUCE on non-global") };
    let Obj::Tuple(args) = args else { bail!("REDUCE args must be a tuple") };
    match name.as_str() {
        "collections.OrderedDict" => Ok(Obj::Dict(Vec::new())),
        "torch._utils._rebuild_tensor_v2" => {
            let Some(Obj::PersId(pid)) = args.first() else { bail!("tensor without storage") };
            let Obj::Tuple(pid) = pid.as_ref() else { bail!("bad storage id") };
            match (pid.first(), pid.get(1), pid.get(2)) {
                (Some(Obj::Str(tag)), Some(Obj::Global(class)), Some(Obj::Str(key)))
                    if tag == "storage" && class == "torch.FloatStorage" =>
                {
                    let Some(Obj::Int(offset)) = args.get(1) else { bail!("bad offset") };
                    Ok(Obj::Tensor {
                        key: key.clone(),
                        offset: *offset as usize,
                        shape: as_usizes(args.get(2).ok_or_else(|| anyhow!("no shape"))?)?,
                        stride: as_usizes(args.get(3).ok_or_else(|| anyhow!("no stride"))?)?,
                    })
                }
                _ => bail!("only float32 storages are supported: {pid:?}"),
            }
        }
        other => bail!("unsupported pickle callable {other}"),
    }
}

fn run_pickle(c: &mut Cursor) -> Result<Obj> {
    let mut stack: Vec<Obj> = Vec::new();
    let mut memo: HashMap<u32, Obj> = HashMap::new();
    let pop = |stack: &mut Vec<Obj>| stack.pop().ok_or_else(|| anyhow!("pickle stack underflow"));
    loop {
        match c.u8()? {
            0x80 => {
                c.u8()?;
            }
            b'.' => return pop(&mut stack),
            b'N' => stack.push(Obj::None),
            0x88 | 0x89 => stack.push(Obj::None),
            b'J' => stack.push(Obj::Int(c.i32()? as i64)),
            b'K' => stack.push(Obj::Int(c.u8()? as i64)),
            b'M' => stack.push(Obj::Int(c.u16()? as i64)),
            0x8a => {
                let n = c.u8()? as usize;
                let raw = c.take(n)?;
                // Only the (ignored) header magic number is ever this long.
                let mut buf = [0u8; 8];
                let m = n.min(8);
                buf[..m].copy_from_slice(&raw[..m]);
                stack.push(Obj::Int(i64::from_le_bytes(buf)));
            }
            b'X' => {
                let len = c.u32()? as usize;
                stack.push(c.string(len)?);
            }
            0x8c => {
                let len = c.u8()? as usize;
                stack.push(c.string(len)?);
            }
            b'}' => stack.push(Obj::Dict(Vec::new())),
            b']' => stack.push(Obj::List(Vec::new())),
            b')' => stack.push(Obj::Tuple(Vec::new())),
            b'(' => stack.push(Obj::Mark),
            0x85 => {
                let a = pop(&mut stack)?;
                stack.push(Obj::Tuple(vec![a]));
            }
            0x86 => {
                let b = pop(&mut stack)?;
                let a = pop(&mut stack)?;
                stack.push(Obj::Tuple(vec![a, b]));
            }
            0x87 => {
                let c3 = pop(&mut stack)?;
                let b = pop(&mut stack)?;
                let a = pop(&mut stack)?;
                stack.push(Obj::Tuple(vec![a, b, c3]));
            }
            b't' => {
                let items = pop_to_mark(&mut stack)?;
                stack.push(Obj::Tuple(items));
            }
            b's' => {
                let v = pop(&mut stack)?;
                let k = pop(&mut stack)?;
                push_pairs(stack.last_mut().ok_or_else(|| anyhow!("empty stack"))?, vec![k, v])?;
            }
            b'u' => {
                let items = pop_to_mark(&mut stack)?;
                push_pairs(stack.last_mut().ok_or_else(|| anyhow!("empty stack"))?, items)?;
            }
            b'a' | b'e' => {
                let items = if c.bytes[c.pos - 1] == b'a' { vec![pop(&mut stack)?] } else { pop_to_mark(&mut stack)? };
                match stack.last_mut() {
                    Some(Obj::List(list)) => list.extend(items),
                    other => bail!("APPEND on non-list {other:?}"),
                }
            }
            b'q' => {
                let idx = c.u8()? as u32;
                memo.insert(idx, stack.last().cloned().ok_or_else(|| anyhow!("empty stack"))?);
            }
            b'r' => {
                let idx = c.u32()?;
                memo.insert(idx, stack.last().cloned().ok_or_else(|| anyhow!("empty stack"))?);
            }
            b'h' | b'j' => {
                let idx = if c.bytes[c.pos - 1] == b'h' { c.u8()? as u32 } else { c.u32()? };
                stack.push(memo.get(&idx).cloned().ok_or_else(|| anyhow!("bad memo index"))?);
            }
            b'c' => {
                let module = c.line()?;
                let name = c.line()?;
                stack.push(Obj::Global(format!("{module}.{name}")));
            }
            b'R' => {
                let args = pop(&mut stack)?;
                let callable = pop(&mut stack)?;
                stack.push(reduce(callable, args)?);
            }
            b'Q' => {
                let pid = pop(&mut stack)?;
                stack.push(Obj::PersId(Box::new(pid)));
            }
            b'b' => {
                pop(&mut stack)?;
            }
            op => bail!("unsupported pickle opcode 0x{op:02x}"),
        }
    }
}

pub fn is_legacy(path: &Path) -> Result<bool> {
    use std::io::Read;
    let mut magic = [0u8; 2];
    std::fs::File::open(path)?.read_exact(&mut magic)?;
    Ok(&magic != b"PK")
}

/// Streams the weights one storage at a time and moves each buffer straight into
/// its tensor, so peak memory stays close to the size of the weights themselves.
pub fn load_state_dict(path: &Path, dev: &Device) -> Result<HashMap<String, Tensor>> {
    use std::io::{BufReader, Read, Seek, SeekFrom};

    if !cfg!(target_endian = "little") {
        bail!("legacy weights are little-endian; big-endian hosts are not supported");
    }
    const HEADER_LIMIT: u64 = 8 << 20;
    let mut file = std::fs::File::open(path)?;
    let mut head = Vec::new();
    (&mut file).take(HEADER_LIMIT).read_to_end(&mut head)?;
    let mut c = Cursor { bytes: &head, pos: 0 };
    for _ in 0..3 {
        run_pickle(&mut c)?;
    }
    let Obj::Dict(entries) = run_pickle(&mut c)? else { bail!("weights root is not a dict") };
    let Obj::List(keys) = run_pickle(&mut c)? else { bail!("storage key list missing") };
    let data_start = c.pos as u64;
    drop(head);

    let mut by_key: HashMap<String, Vec<(String, usize, Vec<usize>)>> = HashMap::new();
    for (name, value) in entries {
        let (Obj::Str(name), Obj::Tensor { key, offset, shape, stride }) = (name, value) else {
            continue;
        };
        let mut expected = 1;
        for (dim, st) in shape.iter().zip(&stride).rev() {
            if *dim != 1 && *st != expected {
                bail!("tensor {name} is not contiguous");
            }
            expected *= dim;
        }
        by_key.entry(key).or_default().push((name, offset, shape));
    }

    file.seek(SeekFrom::Start(data_start))?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut tensors = HashMap::new();
    for key in keys {
        let Obj::Str(key) = key else { bail!("bad storage key") };
        let mut len = [0u8; 8];
        reader.read_exact(&mut len)?;
        let numel = i64::from_le_bytes(len) as usize;
        let Some(users) = by_key.remove(&key) else {
            reader.seek_relative((numel * 4) as i64)?;
            continue;
        };
        let mut storage = vec![0f32; numel];
        reader.read_exact(bytemuck::cast_slice_mut(&mut storage))?;
        if let [(name, 0, shape)] = users.as_slice() {
            if shape.iter().product::<usize>() == numel {
                tensors.insert(name.clone(), Tensor::from_vec(storage, shape.clone(), dev)?);
                continue;
            }
        }
        for (name, offset, shape) in users {
            let n: usize = shape.iter().product();
            let data = storage
                .get(offset..offset + n)
                .ok_or_else(|| anyhow!("tensor {name} exceeds its storage"))?;
            tensors.insert(name, Tensor::from_slice(data, shape, dev)?);
        }
    }
    Ok(tensors)
}
