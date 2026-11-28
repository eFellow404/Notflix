import {
  api,
  byId,
  encodePath,
  formatTime,
  hasValidIntro,
  hasValidOutro,
  parseTimeInput,
  pluralize,
  showToast,
} from "./utils.js";
import {
  getEffectiveDuration,
  getEffectiveTime,
  getEpisodeById,
  setLinkedList,
  state,
  updateEpisodeInState,
} from "./state.js";

let callbacks = {
  onIntroUpdated: () => {},
  onAfterSkipIntro: () => {},
  onSeekTo: null,
};

const INTRO_SOURCE_LABELS = {
  chapters: "chapters",
  audio: "audio match",
  visual: "visual match",
  manual: "manual",
};

const PHASE2_BATCH_SIZE = 8;
const FFMPEG_POLL_INTERVAL_MS = 1500;
const FFMPEG_POLL_MAX_MS = 180000;

let dom = null;
let lastSkipTimeText = "";

function getDom() {
  if (!dom) {
    dom = {
      video: byId("videoPlayer"),
      skipIntroOverlay: byId("skipIntroOverlay"),
      skipIntroTime: byId("skipIntroTime"),
      upNextCard: byId("upNextCard"),
      seekIntroRange: byId("seekIntroRange"),
      seekOutroMarker: byId("seekOutroMarker"),
      metaIntroStatus: byId("metaIntroStatus"),
      introStartInput: byId("introStartInput"),
      introEndInput: byId("introEndInput"),
      introEditorBar: byId("introEditorBar"),
      introEditorError: byId("introEditorError"),
      detectIntrosBtn: byId("detectIntrosBtn"),
    };
  }
  return dom;
}

export function updateSeekIntroHighlight() {
  const { video, seekIntroRange, seekOutroMarker } = getDom();
  const ep = state.currentEpisode;
  const dur = getEffectiveDuration(video);

  if (!hasValidIntro(ep) || !Number.isFinite(dur) || dur <= 0) {
    seekIntroRange.classList.add("hidden");
  } else {
    const startPct = Math.max(0, Math.min(100, (ep.intro_start_sec / dur) * 100));
    const endPct = Math.max(startPct, Math.min(100, (ep.intro_end_sec / dur) * 100));
    seekIntroRange.style.left = `${startPct}%`;
    seekIntroRange.style.width = `${Math.max(0.6, endPct - startPct)}%`;
    seekIntroRange.title = `Intro: ${formatTime(ep.intro_start_sec)} – ${formatTime(ep.intro_end_sec)}`;
    seekIntroRange.classList.remove("hidden");
  }

  if (seekOutroMarker) {
    if (hasValidOutro(ep, dur) && Number.isFinite(dur) && dur > 0) {
      const outroPct = Math.max(0, Math.min(100, (ep.outro_start_sec / dur) * 100));
      seekOutroMarker.style.left = `${outroPct}%`;
      seekOutroMarker.style.width = `${Math.max(0.5, 100 - outroPct)}%`;
      seekOutroMarker.title = `Credits / Outro: ${formatTime(ep.outro_start_sec)}`;
      seekOutroMarker.classList.remove("hidden");
    } else {
      seekOutroMarker.classList.add("hidden");
    }
  }
}

export function updateSkipIntroVisibility() {
  const { video, skipIntroOverlay, skipIntroTime, upNextCard } = getDom();
  const ep = state.currentEpisode;
  const upNextVisible = !upNextCard.classList.contains("hidden");
  let shouldShow = false;

  if (hasValidIntro(ep) && !upNextVisible) {
    const cur = getEffectiveTime(video);
    shouldShow =
      Number.isFinite(cur) && cur >= ep.intro_start_sec && cur < ep.intro_end_sec - 0.5;
  }

  if (shouldShow) {
    const nextText = `→ ${formatTime(ep.intro_end_sec)}`;
    if (nextText !== lastSkipTimeText) {
      lastSkipTimeText = nextText;
      skipIntroTime.textContent = nextText;
    }
    if (skipIntroOverlay.classList.contains("hidden")) {
      skipIntroOverlay.classList.remove("hidden");
    }
  } else if (!skipIntroOverlay.classList.contains("hidden")) {
    skipIntroOverlay.classList.add("hidden");
  }
}

export function skipIntro() {
  const ep = state.currentEpisode;
  if (!hasValidIntro(ep)) return;
  const { video } = getDom();
  const dur = getEffectiveDuration(video);
  const target =
    Number.isFinite(dur) && dur > 1
      ? Math.min(ep.intro_end_sec, dur - 0.5)
      : ep.intro_end_sec;
  const clamped = Math.max(0, target);
  if (callbacks.onSeekTo) {
    callbacks.onSeekTo(clamped);
  } else {
    video.currentTime = clamped;
  }
  updateSkipIntroVisibility();
  callbacks.onAfterSkipIntro();
}

export function refreshIntroUI(statusOverride = null) {
  const { metaIntroStatus, introStartInput, introEndInput } = getDom();
  const ep = state.currentEpisode;
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

async function postDetectIntros(showId, body, isCancelled) {
  const res = await api(`/api/shows/${encodePath(showId)}/detect-intros`, {
    method: "POST",
    body: JSON.stringify(body),
  });
  if (isCancelled()) return null;
  state.activeShow = res.show;
  setLinkedList(res.linked_list);
  syncEpisodeIntroFromLinkedList();
  return res;
}

async function scanReferenceBatch(targets, visual, isCancelled) {
  const heads = [];
  const tails = [];
  for (let i = 0; i < targets.length; i++) {
    if (isCancelled()) return null;
    refreshIntroUI(`Scanning reference episodes (${i + 1}/${targets.length})…`);
    const fp = await visual.extractVisualFingerprint(targets[i], isCancelled);
    if (isCancelled()) return null;
    if (fp) {
      const { tailFingerprint, ...headFp } = fp;
      heads.push(headFp);
      if (tailFingerprint) tails.push(tailFingerprint);
    }
  }
  return { heads, tails };
}

function showVisualScanCompletionToast(totalDetected, silent) {
  if (totalDetected > 0) {
    showToast(`Detected intro on ${pluralize(totalDetected, "episode")}`, "success");
  } else if (!silent) {
    showToast("No shared intro found across episodes — you can mark one manually with Set.");
  }
}

export async function extractVisualFingerprint(episode, isCancelled) {
  const visual = await import("./intro-visual.js");
  return visual.extractVisualFingerprint(episode, isCancelled);
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

  const visual = await import("./intro-visual.js");
  if (isCancelled()) return;

  const curId = state.currentEpisode?.id || null;
  const { refTargets, remainingTargets: initialRemaining } = visual.selectReferenceEpisodes(
    pool,
    curId,
    5
  );
  let remainingTargets = initialRemaining;

  // Phase 1: Dense reference scan on up to 5 evenly-spaced episodes
  const phase1Scan = await scanReferenceBatch(refTargets, visual, isCancelled);
  if (!phase1Scan) return;
  const refCollected = phase1Scan.heads;
  const refTails = phase1Scan.tails;

  if (refCollected.length < 2 && remainingTargets.length === 0) {
    refreshIntroUI("Could not decode ≥2 episodes");
    if (!silent) showToast("Could not decode enough episodes to compare for an intro.");
    return;
  }

  const isValidTemplate = (tmpl) =>
    Boolean(tmpl && Array.isArray(tmpl.vectors) && tmpl.vectors.length > 0);

  let introTemplate = null;
  let totalDetected = 0;
  let didMarkScanned = false;

  if (refCollected.length >= 2) {
    refreshIntroUI("Matching shared intro…");
    const markScannedNow = remainingTargets.length === 0;
    const phase1Body = {
      fingerprints: refCollected,
      force,
      mark_scanned: markScannedNow,
    };
    if (refTails.length >= 2) {
      phase1Body.tail_fingerprints = refTails;
    }
    const phase1Res = await postDetectIntros(showId, phase1Body, isCancelled);
    if (!phase1Res) return;
    if (markScannedNow) didMarkScanned = true;
    totalDetected += phase1Res.detected_count || 0;
    if (isValidTemplate(phase1Res.intro_template)) {
      introTemplate = phase1Res.intro_template;
    }
  }

  // Fallback retry: if no template discovered yet, scan up to 3 more evenly-spaced episodes densely
  if (!introTemplate && remainingTargets.length > 0) {
    const { refTargets: extraRefs, remainingTargets: afterExtra } = visual.selectReferenceEpisodes(
      remainingTargets,
      null,
      3
    );
    remainingTargets = afterExtra;

    const retryScan = await scanReferenceBatch(extraRefs, visual, isCancelled);
    if (!retryScan) return;
    // Place newly scanned episodes first so they are guaranteed to be inside the server's
    // 6-episode reference window even if earlier episodes were intro-less specials/pilots.
    refCollected.unshift(...retryScan.heads);
    refTails.push(...retryScan.tails);

    if (refCollected.length < 2) {
      refreshIntroUI("Could not decode ≥2 episodes");
      if (!silent) showToast("Could not decode enough episodes to compare for an intro.");
      return;
    }

    refreshIntroUI("Matching shared intro…");
    const markScannedNow = remainingTargets.length === 0;
    const retryBody = {
      fingerprints: refCollected,
      force,
      mark_scanned: markScannedNow,
    };
    if (refTails.length >= 2) {
      retryBody.tail_fingerprints = refTails;
    }
    const retryRes = await postDetectIntros(showId, retryBody, isCancelled);
    if (!retryRes) return;
    if (markScannedNow) didMarkScanned = true;
    totalDetected = Math.max(totalDetected, retryRes.detected_count || 0);
    if (isValidTemplate(retryRes.intro_template)) {
      introTemplate = retryRes.intro_template;
    }
  }

  if (!introTemplate) {
    if (!didMarkScanned) {
      try {
        const finalized = await postDetectIntros(
          showId,
          { fingerprints: [], force, mark_scanned: true },
          isCancelled
        );
        if (!finalized) return;
      } catch {
        // Best-effort scan completion marker
      }
    }
    refreshIntroUI();
    showVisualScanCompletionToast(totalDetected, silent);
    return;
  }

  // Phase 2: Fast 5s-block coarse localization + focused fine window across ALL remaining episodes
  if (remainingTargets.length > 0) {
    const anchorTails = refTails.slice(0, 2);
    let pendingFps = [];
    let pendingTails = [];

    for (let i = 0; i < remainingTargets.length; i++) {
      if (isCancelled()) return;
      refreshIntroUI(`Finding intro in episodes (${i + 1}/${remainingTargets.length})…`);
      const extracted = await visual.extractWindowedFingerprint(
        remainingTargets[i],
        introTemplate,
        isCancelled
      );
      if (isCancelled()) return;

      if (extracted) {
        if (extracted.fingerprint) pendingFps.push(extracted.fingerprint);
        if (extracted.tailFingerprint) pendingTails.push(extracted.tailFingerprint);
      }

      const isLast = i === remainingTargets.length - 1;
      if (pendingFps.length >= PHASE2_BATCH_SIZE || isLast) {
        const batchBody = {
          fingerprints: pendingFps,
          intro_template: introTemplate,
          force,
          mark_scanned: isLast,
        };
        if (pendingTails.length >= 2) {
          batchBody.tail_fingerprints = pendingTails;
        } else if (pendingTails.length === 1 && anchorTails.length >= 1) {
          batchBody.tail_fingerprints = [...pendingTails, ...anchorTails];
        }

        const batchRes = await postDetectIntros(showId, batchBody, isCancelled);
        if (!batchRes) return;
        if (isLast) didMarkScanned = true;
        totalDetected += batchRes.detected_count || 0;
        pendingFps = [];
        pendingTails = [];
      }
    }
  }

  refreshIntroUI();
  showVisualScanCompletionToast(totalDetected, silent);
}

async function waitForFfmpegDownload(token, showId, initialProgress) {
  const isCancelled = () =>
    token !== state.introScanToken || !state.activeShow || state.activeShow.id !== showId;

  const updateProgressUI = (rawPct) => {
    const pct = typeof rawPct === "number" && Number.isFinite(rawPct) ? Math.round(rawPct) : null;
    refreshIntroUI(
      pct !== null ? `Downloading audio engine (${pct}%)…` : "Downloading audio engine…"
    );
  };

  updateProgressUI(initialProgress);
  const deadline = Date.now() + FFMPEG_POLL_MAX_MS;

  while (Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, FFMPEG_POLL_INTERVAL_MS));
    if (isCancelled()) return { cancelled: true, ffmpegAvailable: false };

    let health;
    try {
      health = await api("/api/health");
    } catch {
      return { cancelled: isCancelled(), ffmpegAvailable: false };
    }
    if (isCancelled()) return { cancelled: true, ffmpegAvailable: false };

    if (health.ffmpeg_available) {
      return { cancelled: false, ffmpegAvailable: true };
    }
    if (health.ffmpeg_status !== "downloading") {
      return { cancelled: false, ffmpegAvailable: false };
    }
    updateProgressUI(health.ffmpeg_progress);
  }

  return { cancelled: isCancelled(), ffmpegAvailable: false };
}

export async function runIntroDetection(showId, { force = false, silent = false } = {}) {
  const token = ++state.introScanToken;
  const isCancelled = () =>
    token !== state.introScanToken || !state.activeShow || state.activeShow.id !== showId;
  state.introScanning = true;
  const { detectIntrosBtn } = getDom();
  detectIntrosBtn.disabled = true;
  detectIntrosBtn.textContent = "Scanning…";
  refreshIntroUI("Checking chapters & episodes…");

  try {
    let res = await postDetectIntros(showId, { force }, () => token !== state.introScanToken);
    if (!res) return;

    if (res.needs_client_visual_scan && res.ffmpeg_downloading) {
      const waitRes = await waitForFfmpegDownload(token, showId, res.ffmpeg_progress);
      if (waitRes.cancelled) return;

      if (waitRes.ffmpegAvailable) {
        refreshIntroUI("Analyzing episode audio…");
        res = await postDetectIntros(showId, { force }, isCancelled);
        if (!res) return;
      }
    }

    if (res.needs_client_visual_scan) {
      await runBrowserVisualIntroScan(showId, { force, silent, token });
    } else if (res.detected_count > 0) {
      showToast(
        `Detected intros on ${pluralize(res.detected_count, "episode")} (${res.method})`,
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
      detectIntrosBtn.disabled = false;
      detectIntrosBtn.textContent = "✦ Detect";
    }
  }
}

async function saveManualIntro() {
  const ep = state.currentEpisode;
  if (!ep) return false;
  const { introEditorError, introStartInput, introEndInput } = getDom();
  introEditorError.classList.add("hidden");

  const startSec = parseTimeInput(introStartInput.value);
  const endSec = parseTimeInput(introEndInput.value);
  if (startSec === null || endSec === null || endSec <= startSec + 1) {
    introEditorError.textContent = "Enter valid start & end times (e.g. 0:30 and 1:30).";
    introEditorError.classList.remove("hidden");
    return false;
  }

  try {
    const res = await api(`/api/episodes/${encodePath(ep.id)}/intro`, {
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
  const {
    video: videoPlayer,
    introEditorBar,
    introEditorError,
    introStartInput,
    introEndInput,
    detectIntrosBtn,
  } = getDom();
  const skipIntroBtn = byId("skipIntroBtn");

  skipIntroBtn.addEventListener("click", (ev) => {
    ev.stopPropagation();
    skipIntroBtn.blur();
    skipIntro();
  });

  detectIntrosBtn.addEventListener("click", () => {
    if (!state.activeShow || state.introScanning) return;
    runIntroDetection(state.activeShow.id, { force: false, silent: false });
  });

  byId("toggleIntroEditorBtn").addEventListener("click", () => {
    const willOpen = introEditorBar.classList.contains("hidden");
    introEditorBar.classList.toggle("hidden");
    introEditorError.classList.add("hidden");
    if (willOpen && state.currentEpisode) {
      const ep = state.currentEpisode;
      const curTime = getEffectiveTime(videoPlayer);
      introStartInput.value = hasValidIntro(ep)
        ? formatTime(ep.intro_start_sec)
        : formatTime(curTime);
      introEndInput.value = hasValidIntro(ep)
        ? formatTime(ep.intro_end_sec)
        : formatTime(curTime + 60);
      introStartInput.focus();
    }
  });

  byId("closeIntroEditorBtn").addEventListener("click", () => {
    introEditorBar.classList.add("hidden");
  });

  byId("markIntroStartBtn").addEventListener("click", () => {
    introStartInput.value = formatTime(getEffectiveTime(videoPlayer));
  });

  byId("markIntroEndBtn").addEventListener("click", () => {
    introEndInput.value = formatTime(getEffectiveTime(videoPlayer));
  });

  byId("saveIntroBtn").addEventListener("click", async () => {
    const ok = await saveManualIntro();
    if (ok) introEditorBar.classList.add("hidden");
  });

  byId("testIntroBtn").addEventListener("click", async () => {
    const ok = await saveManualIntro();
    if (!ok || !state.currentEpisode) return;
    const start = state.currentEpisode.intro_start_sec || 0;
    const target = Math.max(0, start - 2);
    if (callbacks.onSeekTo) {
      callbacks.onSeekTo(target, { autoPlay: true });
    } else {
      videoPlayer.currentTime = target;
      videoPlayer.play().catch(() => {});
    }
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
      const res = await api(`/api/episodes/${encodePath(ep.id)}/intro`, {
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
