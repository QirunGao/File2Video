use super::*;
use crc32fast::Hasher;
use std::fs;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

fn crc32_file(path: &Path) -> Result<u32> {
    let f = File::open(path).with_context(|| format!("open input {:?}", path))?;
    let mut r = BufReader::new(f);
    let mut hasher = Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf).with_context(|| format!("read input {:?}", path))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

pub(crate) fn encode(
    input: PathBuf,
    out: PathBuf,
    width: usize,
    height: usize,
    fps: u32,
    frames: usize,
    profile: u8,
    use_ffmpeg: bool,
    ffmpeg_path: &str,
    x264_preset: &str,
    x264_crf: u8,
    fragmented_mp4: bool,
    log_every: usize,
) -> Result<()> {
    if profile as usize >= PROFILE_CFGS.len() {
        bail!("profile must be in 0..{}", PROFILE_CFGS.len() - 1);
    }
    let pcfg = cfg(profile);

    if width % 2 != 0 || height % 2 != 0 {
        bail!("YUV420p requires even width/height");
    }
    if width % Y_BLOCK != 0 || height % Y_BLOCK != 0 {
        bail!("width/height must be multiples of Y_BLOCK={} (got {}x{})", Y_BLOCK, width, height);
    }
    if width > u16::MAX as usize || height > u16::MAX as usize {
        bail!("width/height too large (got {}x{})", width, height);
    }
    if fps > u16::MAX as u32 {
        bail!("fps too large (got {})", fps);
    }

    let bx_n = width / Y_BLOCK;
    let by_n = height / Y_BLOCK;
    let boot_layout = build_boot_layout(width, height, pcfg.boot_copies, pcfg.pam_m * pcfg.cal_repeats)?;
    let key_layout = build_profile_layout(width, height, &boot_layout, pcfg)?;
    let tracking_layout = build_tracking_layout(width, height)?;
    let key_payload_bits = payload_capacity_bits(&key_layout, pcfg);
    let tracking_payload_bits = payload_capacity_bits(&tracking_layout, pcfg);
    if key_payload_bits == 0 || tracking_payload_bits == 0 {
        bail!("payload capacity is zero (key={}, tracking={})", key_payload_bits, tracking_payload_bits);
    }

    let meta = fs::metadata(&input).with_context(|| format!("stat input {:?}", input))?;
    let file_len_u64 = meta.len();
    if file_len_u64 == 0 {
        bail!("input is empty");
    }
    if file_len_u64 > MAX_FILE_LEN {
        bail!("input too large: {} bytes > MAX_FILE_LEN {}", file_len_u64, MAX_FILE_LEN);
    }
    let file_crc = crc32_file(&input)?;
    let file_len = file_len_u64 as usize;

    let stream_header = make_stream_header(profile, file_len_u64, file_crc);
    let total_stream_bytes = STREAM_HEADER_LEN
        .checked_add(file_len)
        .context("stream size overflow")?;

    let ldpc = LdpcCode::from_profile(pcfg);
    let cw_payload_bytes = ldpc_user_payload_bytes(&ldpc)?;
    let cw_code_bits = ldpc.n;
    let cw_count = total_stream_bytes.div_ceil(cw_payload_bytes);
    let total_coded_bits = cw_count
        .checked_mul(cw_code_bits)
        .context("coded bit length overflow")?;

    let mut required_frames = 0usize;
    let mut remaining_bits = total_coded_bits;
    while remaining_bits > 0 {
        let frame_idx = required_frames as u32;
        let cap = if frame_uses_boot_cal(frame_idx) {
            payload_capacity_bits_for_frame(&key_layout, pcfg, frame_idx)
        } else {
            payload_capacity_bits_for_frame(&tracking_layout, pcfg, frame_idx)
        };
        if cap == 0 {
            bail!("zero frame payload capacity at frame {}", required_frames);
        }
        remaining_bits = remaining_bits.saturating_sub(cap);
        required_frames += 1;
    }
    if frames < required_frames {
        bail!(
            "frames not enough for LDPC stream: need at least {}, got {} (profile={}, key_payload_bits/frame={}, tracking_payload_bits/frame={}, codeword={} bits, codewords={})",
            required_frames,
            frames,
            profile,
            key_payload_bits,
            tracking_payload_bits,
            cw_code_bits,
            cw_count
        );
    }
    let frames_to_emit = required_frames;
    if frames > frames_to_emit {
        eprintln!(
            "note: requested {} frames but only {} are required; skipping empty tail frames",
            frames,
            frames_to_emit
        );
    }

    let is_stdout_raw = out.as_os_str() == "-";
    let wants_container = is_mp4_like(&out);
    let mut sink = if is_stdout_raw {
        Sink::stdout_raw()
    } else if wants_container && use_ffmpeg {
        Sink::spawn_ffmpeg(
            ffmpeg_path,
            width,
            height,
            fps,
            x264_preset,
            x264_crf,
            fragmented_mp4,
            &out,
        )?
    } else {
        Sink::file_raw(&out)?
    };

    eprintln!(
        "encode(ldpc): file={} bytes, profile={}, ldpc=({},{}) cw_count={}, cw_payload={}B, key_bits/frame={}, track_bits/frame={}, key_intvl={}, frames={} (required={})",
        file_len,
        profile,
        ldpc.n,
        ldpc.k,
        cw_count,
        cw_payload_bytes,
        key_payload_bits,
        tracking_payload_bits,
        BOOT_CAL_KEYFRAME_INTERVAL,
        frames_to_emit,
        required_frames
    );

    let mut frame = Yuv420Frame::new(width, height);
    let mut file_reader = BufReader::new(File::open(&input).with_context(|| format!("open input {:?}", input))?);
    let mut stream_header_off = 0usize;
    let mut stream_bytes_left = total_stream_bytes;
    let mut cw_generated = 0usize;
    let mut cw_payload_buf = Vec::<u8>::with_capacity(cw_payload_bytes);
    let mut info_block_buf = Vec::<u8>::new();
    let mut info_bits_buf = Vec::<u8>::new();
    let mut ldpc_encode_scratch = LdpcEncodeScratch::for_code(&ldpc);
    let mut pending_code_off = 0usize;
    let mut pending_code_valid = 0usize;
    let max_frame_payload_bits = key_payload_bits.max(tracking_payload_bits);
    let mut frame_payload_buf = vec![0u8; max_frame_payload_bits];

    for i in 0..frames_to_emit {
        let tx_frame_idx = i as u32;
        let is_key = frame_uses_boot_cal(tx_frame_idx);
        let layout = if is_key { &key_layout } else { &tracking_layout };
        let frame_payload_bits = payload_capacity_bits_for_frame(layout, pcfg, tx_frame_idx);

        frame.clear(128, 128);
        if is_key {
            draw_locators(&mut frame, width, height, bx_n, by_n, pcfg.locator_size);
            let boot = make_boot(profile, tx_frame_idx);
            write_boot(&mut frame, width, height, &boot_layout, &boot, pcfg.boot_copies);
            draw_cal_strip(&mut frame, width, height, pcfg, &key_layout);
        }

        let payload_bits = &mut frame_payload_buf[..frame_payload_bits];
        payload_bits.fill(0);
        let mut fill_off = 0usize;
        while fill_off < frame_payload_bits {
            if pending_code_off >= pending_code_valid {
                if cw_generated >= cw_count {
                    break;
                }
                let take = cw_payload_bytes.min(stream_bytes_left);
                cw_payload_buf.clear();

                while cw_payload_buf.len() < take && stream_header_off < stream_header.len() {
                    let rem_hdr = stream_header.len() - stream_header_off;
                    let need = take - cw_payload_buf.len();
                    let n = rem_hdr.min(need);
                    cw_payload_buf.extend_from_slice(&stream_header[stream_header_off..stream_header_off + n]);
                    stream_header_off += n;
                }
                while cw_payload_buf.len() < take {
                    let need = take - cw_payload_buf.len();
                    let start = cw_payload_buf.len();
                    cw_payload_buf.resize(start + need, 0);
                    file_reader
                        .read_exact(&mut cw_payload_buf[start..start + need])
                        .with_context(|| format!("read input {:?}", input))?;
                }

                pack_cw_info_block_into(&ldpc, &cw_payload_buf, &mut info_block_buf)?;
                bytes_to_bits_msb_into(&mut info_bits_buf, &info_block_buf);
                let code_bits = ldpc.encode_with_scratch(&info_bits_buf, &mut ldpc_encode_scratch);
                pending_code_valid = code_bits.len();
                pending_code_off = 0;
                cw_generated += 1;
                stream_bytes_left -= take;
            }

            let remain_frame = frame_payload_bits - fill_off;
            let remain_code = pending_code_valid.saturating_sub(pending_code_off);
            let copy_n = remain_frame.min(remain_code);
            if copy_n == 0 {
                break;
            }
            payload_bits[fill_off..fill_off + copy_n]
                .copy_from_slice(&ldpc_encode_scratch.out[pending_code_off..pending_code_off + copy_n]);
            fill_off += copy_n;
            pending_code_off += copy_n;
        }

        write_payload_interleaved_bits(&mut frame, width, height, layout, pcfg, payload_bits, tx_frame_idx);
        sink.write_yuv420p(&frame)?;
        if log_every > 0 && (i + 1) % log_every == 0 {
            eprintln!("encoded {} / {} frames", i + 1, frames_to_emit);
        }
    }

    sink.finish()?;
    eprintln!("done");
    Ok(())
}
