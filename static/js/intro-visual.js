import { encodePath } from "./utils.js";

const LUMA_W = 8;
const LUMA_H = 6;
const LUMA_COUNT = LUMA_W * LUMA_H;
const CHROMA_W = 4;
const CHROMA_H = 3;
const CHROMA_COUNT = CHROMA_W * CHROMA_H;
const VEC_LEN = LUMA_COUNT + CHROMA_COUNT * 2;
const FLAT_LUMA_STD_THRESHOLD = 8.0;
const COARSE_STEP_SEC = 5.0;
const COARSE_NEIGHBOR_DECAY = 0.9;
const FINE_WINDOW_PAD_SEC = 10;

function round1(v) {
  return Math.round(v * 10) / 10 || 0;
}

function computeIntroScanLimit(duration) {
  return duration >= 120 ? Math.min(duration * 0.55, 480) : Math.max(0, duration - 0.5);
}

function normalizeVecJs(raw) {
  if (!raw || raw.length === 0) return null;
  const n = raw.length;
  let sum = 0;
  for (let i = 0; i < n; i++) sum += raw[i];
  const mean = sum / n;
  const out = new Float64Array(n);
  let normSq = 0;
  for (let i = 0; i < n; i++) {
    const c = raw[i] - mean;
    out[i] = c;
    normSq += c * c;
  }
  if (normSq <= 1e-8) return null;
  const invNorm = 1 / Math.sqrt(normSq);
  for (let i = 0; i < n; i++) {
    out[i] *= invNorm;
  }
  return out;
}

function dotVec(a, b) {
  if (!a || !b || a.length !== b.length) return -1;
  let sum = 0;
  for (let i = 0; i < a.length; i++) sum += a[i] * b[i];
  return sum;
}

export function selectReferenceEpisodes(pool, currentId, count = 5) {
  const n = pool.length;
  const targetCount = Math.min(count, n);
  if (targetCount === 0) {
    return { refTargets: [], remainingTargets: [] };
  }

  const curIdx = currentId ? pool.findIndex((e) => e.id === currentId) : -1;
  const slots = [];
  if (targetCount === 1) {
    slots.push(curIdx >= 0 ? curIdx : 0);
  } else {
    for (let k = 0; k < targetCount; k++) {
      slots.push(Math.round((k * (n - 1)) / (targetCount - 1)));
    }
    if (curIdx >= 0 && !slots.includes(curIdx)) {
      let closestSlot = 0;
      let bestDist = Infinity;
      for (let s = 0; s < slots.length; s++) {
        const dist = Math.abs(slots[s] - curIdx);
        if (dist < bestDist) {
          bestDist = dist;
          closestSlot = s;
        }
      }
      slots[closestSlot] = curIdx;
    }
  }

  const selectedIndices = new Set(slots);
  const orderedRefIndices = [];
  if (curIdx >= 0 && selectedIndices.has(curIdx)) {
    orderedRefIndices.push(curIdx);
  }
  const sortedSlots = [...selectedIndices].sort((a, b) => a - b);
  for (const idx of sortedSlots) {
    if (idx !== curIdx) orderedRefIndices.push(idx);
  }

  const refTargets = orderedRefIndices.map((idx) => pool[idx]);
  const remainingTargets = [];
  for (let i = 0; i < n; i++) {
    if (!selectedIndices.has(i)) remainingTargets.push(pool[i]);
  }
  return { refTargets, remainingTargets };
}

function withEpisodeVideo(episode, isCancelled, runner) {
  return new Promise((resolve) => {
    const vid = document.createElement("video");
    vid.muted = true;
    vid.playsInline = true;
    vid.preload = "auto";
    vid.crossOrigin = "anonymous";

    const canvas = document.createElement("canvas");
    canvas.width = LUMA_W;
    canvas.height = LUMA_H;
    const ctx = canvas.getContext("2d", { willReadFrequently: true });

    const activeTimers = new Set();
    const setTrackedTimeout = (fn, ms) => {
      const id = setTimeout(() => {
        activeTimers.delete(id);
        fn();
      }, ms);
      activeTimers.add(id);
      return id;
    };
    const clearTrackedTimeout = (id) => {
      if (id !== null && id !== undefined) {
        clearTimeout(id);
        activeTimers.delete(id);
      }
    };

    let finished = false;
    const cleanup = (result) => {
      if (finished) return;
      finished = true;
      for (const id of activeTimers) clearTimeout(id);
      activeTimers.clear();
      vid.onloadedmetadata = null;
      vid.onerror = null;
      vid.onseeked = null;
      vid.pause();
      vid.removeAttribute("src");
      vid.load();
      resolve(result);
    };

    const loadTimer = setTrackedTimeout(() => cleanup(null), 12000);

    vid.onerror = () => {
      clearTrackedTimeout(loadTimer);
      cleanup(null);
    };

    vid.onloadedmetadata = async () => {
      clearTrackedTimeout(loadTimer);
      if (isCancelled() || !Number.isFinite(vid.duration) || vid.duration < 14) {
        cleanup(null);
        return;
      }

      const duration = vid.duration;
      const scanLimit = computeIntroScanLimit(duration);

      const waitForFrameReady = () =>
        new Promise((res) => {
          if (vid.readyState >= 2) {
            res(true);
            return;
          }
          let done = false;
          const finish = (ready) => {
            if (done) return;
            done = true;
            clearTrackedTimeout(timer);
            vid.removeEventListener("canplay", onReady);
            vid.removeEventListener("loadeddata", onReady);
            res(ready);
          };
          const onReady = () => {
            if (vid.readyState >= 2) finish(true);
          };
          const timer = setTrackedTimeout(() => finish(vid.readyState >= 2), 400);
          vid.addEventListener("canplay", onReady);
          vid.addEventListener("loadeddata", onReady);
        });

      const seekTo = async (t) => {
        if (Math.abs(vid.currentTime - t) < 0.02 && vid.readyState >= 2) {
          return true;
        }
        const seeked = await new Promise((res) => {
          let done = false;
          const timer = setTrackedTimeout(() => {
            if (!done) {
              done = true;
              vid.onseeked = null;
              res(false);
            }
          }, 3500);
          vid.onseeked = () => {
            if (!done) {
              done = true;
              clearTrackedTimeout(timer);
              vid.onseeked = null;
              res(true);
            }
          };
          try {
            vid.currentTime = t;
          } catch {
            clearTrackedTimeout(timer);
            vid.onseeked = null;
            res(false);
          }
        });
        if (!seeked) return false;
        if (vid.readyState >= 2) return true;
        return waitForFrameReady();
      };

      const vw = vid.videoWidth || 0;
      const vh = vid.videoHeight || 0;
      // Crop outer 12% margin so DVD letterbox/pillarbox black bars never bias frame similarity
      const cropX = vw > 0 ? Math.round(vw * 0.12) : 0;
      const cropY = vh > 0 ? Math.round(vh * 0.12) : 0;
      const cropW = vw > 0 ? Math.max(1, vw - cropX * 2) : 0;
      const cropH = vh > 0 ? Math.max(1, vh - cropY * 2) : 0;

      const sampleFrame = async (t) => {
        const ok = await seekTo(t);
        if (!ok || !ctx || vid.readyState < 2) {
          return { vec: new Array(VEC_LEN).fill(0), isFlat: true };
        }
        try {
          if (cropW > 0 && cropH > 0) {
            ctx.drawImage(vid, cropX, cropY, cropW, cropH, 0, 0, LUMA_W, LUMA_H);
          } else {
            ctx.drawImage(vid, 0, 0, LUMA_W, LUMA_H);
          }
          const pixels = ctx.getImageData(0, 0, LUMA_W, LUMA_H).data;

          // 8x6 spatially-centered BT.601 Luma
          const luma = new Float64Array(LUMA_COUNT);
          let sumY = 0;
          for (let i = 0; i < LUMA_COUNT; i++) {
            const p = i * 4;
            const y = 0.299 * pixels[p] + 0.587 * pixels[p + 1] + 0.114 * pixels[p + 2];
            luma[i] = y;
            sumY += y;
          }
          const meanY = sumY / LUMA_COUNT;
          let varY = 0;
          for (let i = 0; i < LUMA_COUNT; i++) {
            const dy = luma[i] - meanY;
            varY += dy * dy;
          }
          const stdY = Math.sqrt(varY / LUMA_COUNT);
          const isFlat = stdY < FLAT_LUMA_STD_THRESHOLD;

          // 4x3 downsampled (2x2 block average) per-channel-centered Cr / Cb chroma
          const cr = new Float64Array(CHROMA_COUNT);
          const cb = new Float64Array(CHROMA_COUNT);
          let sumCr = 0;
          let sumCb = 0;
          let cIdx = 0;
          for (let cy = 0; cy < CHROMA_H; cy++) {
            const y0 = cy * 2;
            const y1 = y0 + 1;
            for (let cx = 0; cx < CHROMA_W; cx++) {
              const x0 = cx * 2;
              const x1 = x0 + 1;
              const i00 = y0 * LUMA_W + x0;
              const i01 = y0 * LUMA_W + x1;
              const i10 = y1 * LUMA_W + x0;
              const i11 = y1 * LUMA_W + x1;
              const avgR =
                (pixels[i00 * 4] + pixels[i01 * 4] + pixels[i10 * 4] + pixels[i11 * 4]) * 0.25;
              const avgB =
                (pixels[i00 * 4 + 2] +
                  pixels[i01 * 4 + 2] +
                  pixels[i10 * 4 + 2] +
                  pixels[i11 * 4 + 2]) *
                0.25;
              const avgY = (luma[i00] + luma[i01] + luma[i10] + luma[i11]) * 0.25;
              const crVal = avgR - avgY;
              const cbVal = avgB - avgY;
              cr[cIdx] = crVal;
              cb[cIdx] = cbVal;
              sumCr += crVal;
              sumCb += cbVal;
              cIdx++;
            }
          }
          const meanCr = sumCr / CHROMA_COUNT;
          const meanCb = sumCb / CHROMA_COUNT;

          const vec = new Array(VEC_LEN);
          for (let i = 0; i < LUMA_COUNT; i++) {
            vec[i] = round1(luma[i] - meanY);
          }
          for (let i = 0; i < CHROMA_COUNT; i++) {
            vec[LUMA_COUNT + i] = round1(cr[i] - meanCr);
          }
          for (let i = 0; i < CHROMA_COUNT; i++) {
            vec[LUMA_COUNT + CHROMA_COUNT + i] = round1(cb[i] - meanCb);
          }
          return { vec, isFlat };
        } catch {
          return { vec: new Array(VEC_LEN).fill(0), isFlat: true };
        }
      };

      const extractTailFingerprint = async () => {
        if (duration < 240) return null;
        const tailStep = 1.5;
        const tailStart = Math.max(0, duration - 120);
        const tailEnd = duration - 0.5;
        const tailVectors = [];
        const tailFlatFlags = [];
        for (let t = tailStart; t < tailEnd; t += tailStep) {
          if (isCancelled()) return null;
          const { vec, isFlat } = await sampleFrame(t);
          tailVectors.push(vec);
          tailFlatFlags.push(isFlat);
        }
        if (tailVectors.length === 0) return null;
        return {
          episode_id: episode.id,
          step_sec: tailStep,
          vectors: tailVectors,
          flat_flags: tailFlatFlags,
          duration_sec: duration,
        };
      };

      try {
        const result = await runner({ duration, scanLimit, sampleFrame, extractTailFingerprint });
        cleanup(result);
      } catch {
        cleanup(null);
      }
    };

    vid.src = `/api/episodes/${encodePath(episode.id)}/stream`;
    vid.load();
  });
}

async function locateTemplateCoarse5s(sampleFrame, scanLimit, template, isCancelled) {
  const tmplVecs = Array.isArray(template?.vectors) ? template.vectors : [];
  const tmplFlats = Array.isArray(template?.flat_flags) ? template.flat_flags : [];
  const tmplWeights = Array.isArray(template?.weights) ? template.weights : [];
  const tmplStep = Number(template?.step_sec) > 0 ? Number(template.step_sec) : 1.0;
  const tmplLen = tmplVecs.length;
  if (tmplLen === 0) return null;

  const tmplDur = tmplLen * tmplStep;
  if (scanLimit < tmplDur * 0.5) return null;

  const activeSamples = [];
  for (let t = 0; t < scanLimit; t += COARSE_STEP_SEC) {
    if (isCancelled()) return null;
    const { vec, isFlat } = await sampleFrame(t);
    if (!isFlat) {
      const normVec = normalizeVecJs(vec);
      if (normVec) {
        activeSamples.push({ t, normVec });
      }
    }
  }
  if (isCancelled() || activeSamples.length < 2) return null;

  const maxOffset = Math.max(0, Math.floor(scanLimit - tmplDur));
  const simThreshold =
    Number.isFinite(template?.sim_threshold) && template.sim_threshold > 0
      ? template.sim_threshold
      : 0.7;
  const prefilter = Math.max(0.42, simThreshold - 0.25);

  let bestOffset = null;
  let bestRankScore = -Infinity;

  for (let offset = 0; offset <= maxOffset; offset++) {
    let weightedSimSum = 0;
    let weightSum = 0;
    let activeHits = 0;

    for (let s = 0; s < activeSamples.length; s++) {
      const sample = activeSamples[s];
      if (sample.t < offset) continue;
      if (sample.t >= offset + tmplDur) break;

      const k = Math.round((sample.t - offset) / tmplStep);
      if (k < 0 || k >= tmplLen) continue;

      let bestSim = -Infinity;
      let bestWeight = 0;

      if (!tmplFlats[k] && tmplVecs[k] && tmplVecs[k].length === sample.normVec.length) {
        bestSim = dotVec(sample.normVec, tmplVecs[k]);
        bestWeight = tmplWeights[k] > 0 ? tmplWeights[k] : 1.0;
      }
      for (let d = -1; d <= 1; d += 2) {
        const nk = k + d;
        if (
          nk >= 0 &&
          nk < tmplLen &&
          !tmplFlats[nk] &&
          tmplVecs[nk] &&
          tmplVecs[nk].length === sample.normVec.length
        ) {
          const sim = dotVec(sample.normVec, tmplVecs[nk]) * COARSE_NEIGHBOR_DECAY;
          if (sim > bestSim) {
            bestSim = sim;
            bestWeight = tmplWeights[nk] > 0 ? tmplWeights[nk] : 1.0;
          }
        }
      }

      if (bestSim > -Infinity) {
        weightedSimSum += bestSim * bestWeight;
        weightSum += bestWeight;
        activeHits += 1;
      }
    }

    if (activeHits < 2 || weightSum <= 0) continue;
    const meanSim = weightedSimSum / weightSum;
    if (meanSim < prefilter) continue;

    const rankScore = meanSim * Math.sqrt(weightSum);
    if (rankScore > bestRankScore) {
      bestRankScore = rankScore;
      bestOffset = offset;
    }
  }

  return bestOffset;
}

export async function extractWindowedFingerprint(episode, template, isCancelled) {
  return withEpisodeVideo(
    episode,
    isCancelled,
    async ({ duration, scanLimit, sampleFrame, extractTailFingerprint }) => {
      const candOffset = await locateTemplateCoarse5s(sampleFrame, scanLimit, template, isCancelled);
      if (isCancelled()) return null;

      let fingerprint = null;
      if (candOffset !== null) {
        const step = Number(template?.step_sec) > 0 ? Number(template.step_sec) : 1.0;
        const tmplDur = (template?.vectors?.length || 0) * step;
        const winStart = Math.max(0, Math.floor(candOffset - FINE_WINDOW_PAD_SEC));
        const winEnd = Math.min(scanLimit, Math.ceil(candOffset + tmplDur + FINE_WINDOW_PAD_SEC));
        const frameCount = Math.max(0, Math.ceil((winEnd - winStart) / step));
        const vectors = [];
        const flatFlags = [];

        for (let i = 0; i < frameCount; i++) {
          if (isCancelled()) return null;
          const t = winStart + i * step;
          if (t >= winEnd) break;
          const { vec, isFlat } = await sampleFrame(t);
          vectors.push(vec);
          flatFlags.push(isFlat);
        }

        if (vectors.length > 0) {
          fingerprint = {
            episode_id: episode.id,
            step_sec: step,
            offset_sec: winStart,
            vectors,
            flat_flags: flatFlags,
            duration_sec: duration,
          };
        }
      }

      const tailFingerprint = await extractTailFingerprint();
      if (isCancelled()) return null;

      return { fingerprint, tailFingerprint };
    }
  );
}

export async function extractVisualFingerprint(episode, isCancelled) {
  return withEpisodeVideo(
    episode,
    isCancelled,
    async ({ duration, scanLimit, sampleFrame, extractTailFingerprint }) => {
      const step = 1.0;
      const vectors = [];
      const flatFlags = [];

      for (let t = 0; t < scanLimit; t += step) {
        if (isCancelled()) return null;
        const { vec, isFlat } = await sampleFrame(t);
        vectors.push(vec);
        flatFlags.push(isFlat);
      }

      const tailFingerprint = await extractTailFingerprint();
      if (isCancelled()) return null;

      return {
        episode_id: episode.id,
        step_sec: step,
        offset_sec: 0,
        vectors,
        flat_flags: flatFlags,
        duration_sec: duration,
        tailFingerprint,
      };
    }
  );
}
