use super::*;
use std::fs;
use std::path::PathBuf;

pub(crate) fn decode(
    input: PathBuf,
    out: PathBuf,
    width: Option<usize>,
    height: Option<usize>,
    use_ffmpeg: bool,
    ffmpeg_path: &str,
    ffprobe_path: &str,
    max_frames: usize,
    log_every: usize,
    heavy_geom: bool,
) -> Result<()> {
    let (w, h) = if is_mp4_like(&input) && use_ffmpeg {
        probe_video_wh(ffprobe_path, &input).with_context(|| "ffprobe failed")?
    } else {
        let w = width.context("width is required for raw yuv input")?;
        let h = height.context("height is required for raw yuv input")?;
        (w, h)
    };

    if w % 2 != 0 || h % 2 != 0 {
        bail!("YUV420p requires even width/height");
    }
    if w % Y_BLOCK != 0 || h % Y_BLOCK != 0 {
        bail!("width/height must be multiples of Y_BLOCK={} (got {}x{})", Y_BLOCK, w, h);
    }

    let mut source = if is_mp4_like(&input) && use_ffmpeg {
        Source::spawn_ffmpeg_decode(ffmpeg_path, &input)?
    } else {
        Source::file_raw(&input)?
    };

    let mut frame = Yuv420Frame::new(w, h);
    let bx_n = w / Y_BLOCK;
    let by_n = h / Y_BLOCK;
    let mut boot_layout = build_boot_layout(w, h, 1, cal_max_blocks_all_profiles())?;

    eprintln!(
        "decode(ldpc): input={:?}, {}x{}, max_frames={}, heavy_geom={}",
        input,
        w,
        h,
        max_frames,
        heavy_geom
    );

    let mut locked_profile: Option<u8> = None;
    let mut key_layout: Option<ProfileLayout> = None;
    let mut tracking_layout: Option<ProfileLayout> = None;
    let mut ldpc: Option<LdpcCode> = None;
    let mut ldpc_decode_scratch: Option<LdpcDecodeScratch> = None;

    let mut llr_assembler: Option<LlrAssembler> = None;
    let mut stream_bytes = Vec::<u8>::new();
    let mut decoded_cw = 0usize;
    let mut info_bytes_buf = Vec::<u8>::new();
    let mut payload_llr_raw_scratch = Vec::<f32>::new();

    let mut header: Option<StreamHeader> = None;
    let mut total_stream_bytes_exact: Option<usize> = None;
    let mut total_cw_needed: Option<usize> = None;
    let mut geom_fusion = if heavy_geom { Some(GeomFusionState::default()) } else { None };
    let mut tracked_geom: Option<Geom> = None;
    let mut tracked_means: Option<Means> = None;

    for i in 0..max_frames {
        let ok = source.read_yuv420p(&mut frame)?;
        if !ok {
            break;
        }

        let boot_locator_size = locked_profile
            .map(|p| cfg(p).locator_size)
            .unwrap_or_else(|| PROFILE_CFGS.iter().map(|p| p.locator_size).max().unwrap_or(1));

        let geom_hint = geom_fusion.as_ref().and_then(|s| s.hint());
        let boot_info = read_boot(&frame, &boot_layout, bx_n, by_n, boot_locator_size, heavy_geom, geom_hint);
        let prof_from_boot = boot_info
            .as_ref()
            .map(|b| b.profile)
            .filter(|&p| (p as usize) < PROFILE_CFGS.len());

        if locked_profile.is_none() {
            if let Some(p) = prof_from_boot {
                let pcfg = cfg(p);
                boot_layout = build_boot_layout(w, h, pcfg.boot_copies, pcfg.pam_m * pcfg.cal_repeats)?;
                let lk = build_profile_layout(w, h, &boot_layout, pcfg)?;
                let lt = build_tracking_layout(w, h)?;
                let key_frame_payload_bits = payload_capacity_bits(&lk, pcfg);
                let tracking_frame_payload_bits = payload_capacity_bits(&lt, pcfg);
                if key_frame_payload_bits == 0 || tracking_frame_payload_bits == 0 {
                    bail!("payload capacity is zero");
                }
                let code = LdpcCode::from_profile(pcfg);
                let cw_payload_bytes = ldpc_user_payload_bytes(&code)?;
                ldpc_decode_scratch = Some(LdpcDecodeScratch::for_code(&code));
                llr_assembler = Some(LlrAssembler::new(code.n));
                ldpc = Some(code);
                key_layout = Some(lk);
                tracking_layout = Some(lt);
                locked_profile = Some(p);
                eprintln!(
                    "locked profile={}, ldpc=({},{}) cw_payload={}B key_bits/frame={} track_bits/frame={} key_intvl={}",
                    p,
                    pcfg.ldpc_n,
                    pcfg.ldpc_k,
                    cw_payload_bytes,
                    key_frame_payload_bits,
                    tracking_frame_payload_bits,
                    BOOT_CAL_KEYFRAME_INTERVAL
                );
            } else {
                if log_every > 0 && (i + 1) % log_every == 0 {
                    eprintln!("decoded {} frames, waiting for BOOT/profile lock", i + 1);
                }
                continue;
            }
        }

        let p = locked_profile.unwrap();
        let pcfg = cfg(p);
        let l_key = key_layout.as_ref().unwrap();
        let l_track = tracking_layout.as_ref().unwrap();
        let code = ldpc.as_ref().unwrap();
        let llr_asm = llr_assembler.as_mut().expect("llr assembler initialized");
        let ldpc_scratch = ldpc_decode_scratch.as_mut().expect("ldpc scratch initialized");
        let expected_tx_frame_idx = llr_asm.next_expected_frame().unwrap_or(i as u32);
        let tx_frame_idx = boot_info.as_ref().map(|b| b.frame_idx).unwrap_or(expected_tx_frame_idx);
        let is_key_frame = frame_uses_boot_cal(tx_frame_idx);
        let l = if is_key_frame { l_key } else { l_track };
        let frame_payload_bits = payload_capacity_bits_for_frame(l, pcfg, tx_frame_idx);

        let mut frame_llr = vec![0f32; frame_payload_bits]; // erasure by default

        let boot_ok = prof_from_boot.map(|bp| bp == p).unwrap_or(false);
        if boot_ok {
            if let Some(boot) = boot_info.as_ref() {
                let fused_geom = if let (Some(fusion), Some(est)) = (geom_fusion.as_mut(), boot.geom_est) {
                    let fused_est = fusion.fuse(est);
                    if log_every > 0 && (i + 1) % log_every == 0 {
                        eprintln!(
                            "geom(heavy): conf={:.3} gap={:.3} score={:.1}/{:.1} dx={:.2} dy={:.2} sx={:.4} sy={:.4}",
                            fused_est.confidence,
                            fused_est.score_gap,
                            fused_est.best_score,
                            fused_est.second_best_score,
                            fused_est.geom.dx,
                            fused_est.geom.dy,
                            fused_est.geom.sx,
                            fused_est.geom.sy
                        );
                    }
                    fused_est.geom
                } else {
                    boot.geom
                };
                tracked_geom = Some(fused_geom);
                let cache = AlignedFrameCache::build(&frame, fused_geom);
                if is_key_frame {
                    if let Some(m) = read_cal_means_cached(&cache, pcfg, l_key) {
                        tracked_means = Some(m);
                    }
                }
                if let Some(means) = tracked_means.as_ref() {
                    let _ = read_payload_interleaved_llrs_cached_into(
                        &cache,
                        means,
                        l,
                        pcfg,
                        frame_payload_bits,
                        tx_frame_idx,
                        &mut frame_llr,
                        &mut payload_llr_raw_scratch,
                    );
                }
            }
        } else if let (Some(g), Some(means)) = (tracked_geom, tracked_means.as_ref()) {
            let cache = AlignedLumaCache::build(&frame, g);
            let _ = read_payload_interleaved_llrs_luma_cached_into(
                &cache,
                means,
                l,
                pcfg,
                frame_payload_bits,
                tx_frame_idx,
                &mut frame_llr,
                &mut payload_llr_raw_scratch,
            );
        }
        let seq_for_buffer = boot_info
            .as_ref()
            .map(|b| b.frame_idx)
            .or(llr_asm.next_expected_frame())
            .unwrap_or(i as u32);
        let is_first_seq = llr_asm.next_expected_frame().is_none();
        if is_first_seq && seq_for_buffer != 0 {
            eprintln!("warning: first decoded frame_idx={} (stream may be truncated)", seq_for_buffer);
        }
        llr_asm.append_frame_llr(seq_for_buffer, frame_llr);
        while let Some(codeword_llr) = llr_asm.next_codeword() {
            if let Some(limit) = total_cw_needed {
                if decoded_cw >= limit {
                    break;
                }
            }

            let Some(info_chunk) = code.decode_soft_with_scratch(codeword_llr.as_slice(), pcfg.ldpc_iters, ldpc_scratch) else {
                bail!("LDPC decode failed at codeword {}", decoded_cw);
            };
            bits_to_bytes_msb_into(&mut info_bytes_buf, &info_chunk);
            let Some(cw_payload) = unpack_cw_info_block_view(code, &info_bytes_buf) else {
                bail!("invalid codeword envelope (CRC or format) at codeword {}", decoded_cw);
            };
            stream_bytes.extend_from_slice(&cw_payload);
            decoded_cw += 1;

            if header.is_none() && stream_bytes.len() >= STREAM_HEADER_LEN {
                let Some(h) = parse_stream_header(&stream_bytes[..STREAM_HEADER_LEN]) else {
                    bail!("invalid stream header after LDPC decode");
                };
                if h.profile != p {
                    bail!("stream header profile mismatch: boot={} header={}", p, h.profile);
                }
                if h.file_len == 0 || h.file_len > MAX_FILE_LEN {
                    bail!("stream header file_len out of range: {}", h.file_len);
                }
                let total_bytes = STREAM_HEADER_LEN
                    .checked_add(h.file_len as usize)
                    .context("stream size overflow")?;
                let cw_payload_bytes = ldpc_user_payload_bytes(code)?;
                let cw_needed = (total_bytes + cw_payload_bytes - 1) / cw_payload_bytes;
                if stream_bytes.capacity() < total_bytes {
                    stream_bytes.reserve(total_bytes - stream_bytes.capacity());
                }
                total_stream_bytes_exact = Some(total_bytes);
                total_cw_needed = Some(cw_needed);
                header = Some(h.clone());
                eprintln!(
                    "stream header: profile={}, file_len={}, file_crc=0x{:08x}, codewords={}",
                    h.profile,
                    h.file_len,
                    h.file_crc,
                    cw_needed
                );
            }

            if let (Some(h), Some(total_bytes), Some(cw_needed)) = (&header, total_stream_bytes_exact, total_cw_needed) {
                if decoded_cw >= cw_needed {
                    if stream_bytes.len() < total_bytes {
                        bail!("decoded stream shorter than header announced");
                    }
                    let file = &stream_bytes[STREAM_HEADER_LEN..STREAM_HEADER_LEN + h.file_len as usize];
                    let got = crc32(file);
                    if got != h.file_crc {
                        bail!(
                            "recovered file CRC mismatch: got=0x{:08x}, expected=0x{:08x}",
                            got,
                            h.file_crc
                        );
                    }
                    fs::write(&out, file).with_context(|| format!("write output {:?}", out))?;
                    eprintln!(
                        "done: recovered {} bytes to {:?} (frames_used={}, codewords={})",
                        file.len(),
                        out,
                        i + 1,
                        decoded_cw
                    );
                    source.finish()?;
                    return Ok(());
                }
            }
        }

        if log_every > 0 && (i + 1) % log_every == 0 {
            eprintln!(
                        "decoded {} frames, profile_locked={}, llr_bits={}, codewords_decoded={}, stream_bytes={}",
                        i + 1,
                        locked_profile.is_some(),
                        llr_asm.available_llr_len(),
                        decoded_cw,
                        stream_bytes.len()
                    );
        }
    }

    source.finish()?;
    bail!(
        "decode not complete (profile_locked={}, codewords_decoded={}, header={})",
        locked_profile.is_some(),
        decoded_cw,
        header.is_some()
    )
}
