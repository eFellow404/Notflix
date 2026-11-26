import { api, byId, formatTime, hasValidIntro, parseTimeInput, showToast } from "./utils.js";
import { getEpisodeById, setLinkedList, state, updateEpisodeInState } from "./state.js";

let callbacks = {
  onIntroUpdated: () => {},
  onAfterSkipIntro: () => {},
};

const INTRO_SOURCE_LABELS = {
  chapters: "chapters",
  audio: "audio match",
  visual: "visual match",
  manual: "manual",
};

export function updateSeekIntroHighlight() {
  const ep = state.currentEpisode;
  const seekIntroRange = byId("seekIntroRange");
  const dur = byId("videoPlayer").duration;
  if (!hasValidIntro(ep) || !Number.isFinite(dur) || dur <= 0) {
    seekIntroRange.classList.add("hidden");
    return;
  }
  const startPct = Math.max(0, Math.min(100, (ep.intro_start_sec / dur) * 100));
  const endPct = Math.max(startPct, Math.min(100, (ep.intro_end_sec / dur) * 100));
  seekIntroRange.style.left = `${startPct}%`;
  seekIntroRange.style.width = `${Math.max(0.6, endPct - startPct)}%`;
  seekIntroRange.title = `Intro: ${formatTime(ep.intro_start_sec)} – ${formatTime(ep.intro_end_sec)}`;
  seekIntroRange.classList.remove("hidden");
}

export function updateSkipIntroVisibility() {
  const ep = state.currentEpisode;
  const skipIntroOverlay = byId("skipIntroOverlay");
  const upNextVisible = !byId("upNextCard").classList.contains("hidden");
  if (!hasValidIntro(ep) || upNextVisible) {
    skipIntroOverlay.classList.add("hidden");
    return;
  }
  const cur = byId("videoPlayer").currentTime;
  const inIntro =
    Number.isFinite(cur) &&
    cur >= ep.intro_start_sec &&
    cur < ep.intro_end_sec - 0.5;

  if (inIntro) {
    byId("skipIntroTime").textContent = `→ ${formatTime(ep.intro_end_sec)}`;
    skipIntroOverlay.classList.remove("hidden");
  } else {
    skipIntroOverlay.classList.add("hidden");
  }
}

export function skipIntro() {
  const ep = state.currentEpisode;
  if (!hasValidIntro(ep)) return;
  const video = byId("videoPlayer");
  const dur = video.duration;
  const target =
    Number.isFinite(dur) && dur > 1
      ? Math.min(ep.intro_end_sec, dur - 0.5)
      : ep.intro_end_sec;
  video.currentTime = Math.max(0, target);
  updateSkipIntroVisibility();
  callbacks.onAfterSkipIntro();
}

export function refreshIntroUI(statusOverride = null) {
  const ep = state.currentEpisode;
  const metaIntroStatus = byId("metaIntroStatus");
  if (statusOverride) {
    metaIntroStatus.textContent = statusOverride;
  } else if (hasValidIntro(ep)) {
    const srcLabel = INTRO_SOURCE_LABELS[ep.intro_source] || "detected";
    const pct = ep.intro_confidence > 0 ? ` · ${Math.round(ep.intro_confidence * 100)}%` : "";
    metaIntroStatus.textContent = `${formatTime(ep.intro_start_sec)} – ${formatTime(ep.intro_end_sec)} (${srcLabel}${pct})`;
  } else if (state.linkedList && state.linkedList.episodes.length < 2) {
    metaIntroStatus.textContent = "Single episode (click Set to add)";
  } else {
    metaIntroStatus.textContent = "Not detected yet";
  }

  const introStartInput = byId("introStartInput");
  const introEndInput = byId("introEndInput");
  if (hasValidIntro(ep)) {
    introStartInput.value = formatTime(ep.intro_start_sec);
    introEndInput.value = formatTime(ep.intro_end_sec);
  } else {
    introStartInput.value = "";
    introEndInput.value = "";
  }

  updateSeekIntroHighlight();
  updateSkipIntroVisibility();
}

function syncEpisodeIntroFromLinkedList() {
  if (!state.linkedList || !state.currentEpisode) return;
  const updated = getEpisodeById(state.currentEpisode.id);
  if (updated) {
    state.currentEpisode = updated;
  }
  refreshIntroUI();
  callbacks.onIntroUpdated();
}

export async function extractVisualFingerprint(episode, isCancelled) {
  return new Promise((resolve) => {
    const vid = document.createElement("video");
    vid.muted = true;
    vid.playsInline = true;
    vid.preload = "auto";
    vid.crossOrigin = "anonymous";

    const GRID_W = 6;
    const GRID_H = 4;
    const VEC_LEN = GRID_W * GRID_H * 3;
    const canvas = document.createElement("canvas");
    canvas.width = GRID_W;
    canvas.height = GRID_H;
    const ctx = canvas.getContext("2d", { willReadFrequently: true });

    let finished = false;
    const cleanup = (result) => {
      if (finished) return;
      finished = true;
      vid.onloadedmetadata = null;
      vid.onerror = null;
      vid.onseeked = null;
      vid.pause();
      vid.removeAttribute("src");
      vid.load();
      resolve(result);
    };

    const loadTimer = setTimeout(() => cleanup(null), 12000);

    vid.onerror = () => {
      clearTimeout(loadTimer);
      cleanup(null);
    };

    vid.onloadedmetadata = async () => {
      clearTimeout(loadTimer);
      if (isCancelled() || !Number.isFinite(vid.duration) || vid.duration < 14) {
        cleanup(null);
        return;
      }

      const duration = vid.duration;
      const scanLimit = Math.min(duration - 0.5, 360);
      const step = 1.0;
      const vectors = [];
      const flatFlags = [];

      const seekTo = (t) =>
        new Promise((res) => {
          if (Math.abs(vid.currentTime - t) < 0.02 && vid.readyState >= 2) {
            res(true);
            return;
          }
          let done = false;
          const timer = setTimeout(() => {
            if (!done) {
              done = true;
              vid.onseeked = null;
              res(false);
            }
          }, 3500);
          vid.onseeked = () => {
            if (!done) {
              done = true;
              clearTimeout(timer);
              vid.onseeked = null;
              res(true);
            }
          };
          try {
            vid.currentTime = t;
          } catch {
            clearTimeout(timer);
            res(false);
          }
        });

      const vw = vid.videoWidth || 0;
      const vh = vid.videoHeight || 0;
      // Crop outer 12% margin so DVD letterbox/pillarbox black bars never bias frame similarity
      const cropX = vw > 0 ? Math.round(vw * 0.12) : 0;
      const cropY = vh > 0 ? Math.round(vh * 0.12) : 0;
      const cropW = vw > 0 ? Math.max(1, vw - cropX * 2) : 0;
      const cropH = vh > 0 ? Math.max(1, vh - cropY * 2) : 0;

      for (let t = 0; t < scanLimit; t += step) {
        if (isCancelled()) {
          cleanup(null);
          return;
        }
        const ok = await seekTo(t);
        if (!ok || !ctx) {
          vectors.push(new Array(VEC_LEN).fill(0));
          flatFlags.push(true);
          continue;
        }
        try {
          if (cropW > 0 && cropH > 0) {
            ctx.drawImage(vid, cropX, cropY, cropW, cropH, 0, 0, GRID_W, GRID_H);
          } else {
            ctx.drawImage(vid, 0, 0, GRID_W, GRID_H);
          }
          const pixels = ctx.getImageData(0, 0, GRID_W, GRID_H).data;
          const vec = [];
          let sum = 0;
          for (let p = 0; p < pixels.length; p += 4) {
            const r = pixels[p];
            const g = pixels[p + 1];
            const b = pixels[p + 2];
            vec.push(r, g, b);
            sum += r + g + b;
          }
          const mean = sum / vec.length;
          let varSum = 0;
          for (let k = 0; k < vec.length; k++) {
            const d = vec[k] - mean;
            varSum += d * d;
          }
          vectors.push(vec);
          flatFlags.push(Math.sqrt(varSum / vec.length) < 8.0);
        } catch {
          vectors.push(new Array(VEC_LEN).fill(0));
          flatFlags.push(true);
        }
      }

      cleanup({
        episode_id: episode.id,
        step_sec: step,
        vectors,
        flat_flags: flatFlags,
        duration_sec: duration,
      });
    };

    vid.src = `/api/episodes/${episode.id}/stream`;
    vid.load();
  });
}

export async function runBrowserVisualIntroScan(showId, { force = false, silent = false, token }) {
  const isCancelled = () =>
    token !== state.introScanToken || !state.activeShow || state.activeShow.id !== showId;

  const allEps = state.linkedList ? state.linkedList.episodes : [];
  const nonExtras = allEps.filter((e) => !e.is_extra);
  const pool = nonExtras.length >= 2 ? nonExtras : allEps;
  if (pool.length < 2) {
    if (!silent) showToast("Need at least 2 episodes to compare for a shared intro.");
    refreshIntroUI();
    return;
  }

  const curId = state.currentEpisode?.id;
  const prioritized = [...pool];
  if (curId) {
    const idx = prioritized.findIndex((e) => e.id === curId);
    if (idx > 0) {
      const [curEp] = prioritized.splice(idx, 1);
      prioritized.unshift(curEp);
    }
  }
  const targets = prioritized.slice(0, 24);
  const collected = [];
  const firstBatchSize = Math.min(4, targets.length);

  for (let i = 0; i < targets.length; i++) {
    if (isCancelled()) return;
    const ep = targets[i];
    refreshIntroUI(`Scanning episodes (${i + 1}/${targets.length})…`);
    const fp = await extractVisualFingerprint(ep, isCancelled);
    if (isCancelled()) return;
    if (fp) collected.push(fp);

    if (i + 1 === firstBatchSize && targets.length > firstBatchSize && collected.length >= 2) {
      try {
        const partial = await api(`/api/shows/${showId}/detect-intros`, {
          method: "POST",
          body: JSON.stringify({
            fingerprints: collected,
            force,
            mark_scanned: false,
          }),
        });
        if (isCancelled()) return;
        state.activeShow = partial.show;
        setLinkedList(partial.linked_list);
        syncEpisodeIntroFromLinkedList();
      } catch {
        // Continue to final pass
      }
    }
  }

  if (isCancelled()) return;
  if (collected.length < 2) {
    refreshIntroUI("Could not decode ≥2 episodes");
    if (!silent) showToast("Could not decode enough episodes to compare for an intro.");
    return;
  }

  refreshIntroUI("Matching shared intro…");
  const finalRes = await api(`/api/shows/${showId}/detect-intros`, {
    method: "POST",
    body: JSON.stringify({
      fingerprints: collected,
      force,
      mark_scanned: true,
    }),
  });
  if (isCancelled()) return;

  state.activeShow = finalRes.show;
  setLinkedList(finalRes.linked_list);
  syncEpisodeIntroFromLinkedList();

  if (finalRes.detected_count > 0) {
    showToast(
      `Detected intro on ${finalRes.detected_count} episode${finalRes.detected_count === 1 ? "" : "s"}`,
      "success"
    );
  } else if (!silent) {
    showToast("No shared intro found across episodes — you can mark one manually with Set.");
  }
}

export async function runIntroDetection(showId, { force = false, silent = false } = {}) {
  const token = ++state.introScanToken;
  state.introScanning = true;
  const detectBtn = byId("detectIntrosBtn");
  detectBtn.disabled = true;
  detectBtn.textContent = "Scanning…";
  refreshIntroUI("Checking chapters & episodes…");

  try {
    const res = await api(`/api/shows/${showId}/detect-intros`, {
      method: "POST",
      body: JSON.stringify({ force }),
    });
    if (token !== state.introScanToken) return;

    state.activeShow = res.show;
    setLinkedList(res.linked_list);
    syncEpisodeIntroFromLinkedList();

    if (res.needs_client_visual_scan) {
      await runBrowserVisualIntroScan(showId, { force, silent, token });
    } else if (res.detected_count > 0) {
      showToast(
        `Detected intros on ${res.detected_count} episode${res.detected_count === 1 ? "" : "s"} (${res.method})`,
        "success"
      );
    } else if (!silent) {
      showToast("No shared intro detected — click Set to mark it manually.");
    }
  } catch (err) {
    if (token === state.introScanToken) {
      refreshIntroUI();
      if (!silent) showToast(`Intro detection failed: ${err.message}`);
    }
  } finally {
    if (token === state.introScanToken) {
      state.introScanning = false;
      detectBtn.disabled = false;
      detectBtn.textContent = "✦ Detect";
    }
  }
}

async function saveManualIntro() {
  const ep = state.currentEpisode;
  if (!ep) return false;
  const introEditorError = byId("introEditorError");
  introEditorError.classList.add("hidden");

  const startSec = parseTimeInput(byId("introStartInput").value);
  const endSec = parseTimeInput(byId("introEndInput").value);
  if (startSec === null || endSec === null || endSec <= startSec + 1) {
    introEditorError.textContent = "Enter valid start & end times (e.g. 0:30 and 1:30).";
    introEditorError.classList.remove("hidden");
    return false;
  }

  try {
    const res = await api(`/api/episodes/${ep.id}/intro`, {
      method: "PATCH",
      body: JSON.stringify({ intro_start_sec: startSec, intro_end_sec: endSec }),
    });
    updateEpisodeInState(res.episode);
    refreshIntroUI();
    callbacks.onIntroUpdated();
    showToast(`Saved intro: ${formatTime(startSec)} – ${formatTime(endSec)}`, "success");
    return true;
  } catch (err) {
    introEditorError.textContent = err.message;
    introEditorError.classList.remove("hidden");
    return false;
  }
}

export function initIntro(hooks = {}) {
  callbacks = { ...callbacks, ...hooks };

  const videoPlayer = byId("videoPlayer");
  const introEditorBar = byId("introEditorBar");
  const introEditorError = byId("introEditorError");
  const introStartInput = byId("introStartInput");
  const introEndInput = byId("introEndInput");
  const skipIntroBtn = byId("skipIntroBtn");

  skipIntroBtn.addEventListener("click", (ev) => {
    ev.stopPropagation();
    skipIntroBtn.blur();
    skipIntro();
  });

  byId("detectIntrosBtn").addEventListener("click", () => {
    if (!state.activeShow || state.introScanning) return;
    runIntroDetection(state.activeShow.id, { force: false, silent: false });
  });

  byId("toggleIntroEditorBtn").addEventListener("click", () => {
    const willOpen = introEditorBar.classList.contains("hidden");
    introEditorBar.classList.toggle("hidden");
    introEditorError.classList.add("hidden");
    if (willOpen && state.currentEpisode) {
      const ep = state.currentEpisode;
      introStartInput.value = hasValidIntro(ep)
        ? formatTime(ep.intro_start_sec)
        : formatTime(videoPlayer.currentTime || 0);
      introEndInput.value = hasValidIntro(ep)
        ? formatTime(ep.intro_end_sec)
        : formatTime((videoPlayer.currentTime || 0) + 60);
      introStartInput.focus();
    }
  });

  byId("closeIntroEditorBtn").addEventListener("click", () => {
    introEditorBar.classList.add("hidden");
  });

  byId("markIntroStartBtn").addEventListener("click", () => {
    introStartInput.value = formatTime(videoPlayer.currentTime || 0);
  });

  byId("markIntroEndBtn").addEventListener("click", () => {
    introEndInput.value = formatTime(videoPlayer.currentTime || 0);
  });

  byId("saveIntroBtn").addEventListener("click", async () => {
    const ok = await saveManualIntro();
    if (ok) introEditorBar.classList.add("hidden");
  });

  byId("testIntroBtn").addEventListener("click", async () => {
    const ok = await saveManualIntro();
    if (!ok || !state.currentEpisode) return;
    const start = state.currentEpisode.intro_start_sec || 0;
    videoPlayer.currentTime = Math.max(0, start - 2);
    videoPlayer.play().catch(() => {});
    updateSkipIntroVisibility();
  });

  for (const input of [introStartInput, introEndInput]) {
    input.addEventListener("keydown", async (ev) => {
      if (ev.key === "Enter") {
        const ok = await saveManualIntro();
        if (ok) introEditorBar.classList.add("hidden");
      }
    });
  }

  byId("clearIntroBtn").addEventListener("click", async () => {
    const ep = state.currentEpisode;
    if (!ep) return;
    try {
      const res = await api(`/api/episodes/${ep.id}/intro`, {
        method: "PATCH",
        body: JSON.stringify({ clear: true }),
      });
      updateEpisodeInState(res.episode);
      introEditorBar.classList.add("hidden");
      refreshIntroUI();
      callbacks.onIntroUpdated();
      showToast("Cleared intro for this episode");
    } catch (err) {
      showToast(`Failed to clear intro: ${err.message}`);
    }
  });
}
