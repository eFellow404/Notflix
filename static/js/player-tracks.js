import {
  api,
  byId,
  encodePath,
  parseErrorResponse,
  showToast,
  storageGet,
  storageSet,
} from "./utils.js";
import { getEffectiveTime, state } from "./state.js";

const SUBTITLE_PREF_KEY = "notflix.subtitles";

let trackCallbacks = {
  onControlsUpdate: () => {},
};

export function initPlayerTracks(hooks = {}) {
  trackCallbacks = { ...trackCallbacks, ...hooks };
}

export function readSubtitlePref() {
  try {
    const raw = storageGet(SUBTITLE_PREF_KEY);
    if (!raw) return { enabled: false, lang: "" };
    const parsed = JSON.parse(raw);
    return {
      enabled: Boolean(parsed?.enabled),
      lang: typeof parsed?.lang === "string" ? parsed.lang : "",
    };
  } catch {
    return { enabled: false, lang: "" };
  }
}

export function saveSubtitlePref(enabled, lang = "") {
  storageSet(SUBTITLE_PREF_KEY, JSON.stringify({ enabled: Boolean(enabled), lang: lang || "" }));
}

export function syncSubtitleTrackModes() {
  const video = byId("videoPlayer");
  const tracks = video.textTracks;
  if (tracks) {
    for (let i = 0; i < tracks.length; i++) {
      tracks[i].mode = i === state.activeSubtitleIndex ? "showing" : "disabled";
    }
  }
  const btn = byId("ctrlSubBtn");
  if (!btn) return;
  const hasSubs = state.subtitleTracks.length > 0;
  btn.classList.toggle("hidden", !hasSubs);
  const enabled = hasSubs && state.activeSubtitleIndex >= 0;
  btn.classList.toggle("active", enabled);
  btn.classList.toggle("is-on", enabled);
  if (enabled) {
    const t = state.subtitleTracks[state.activeSubtitleIndex];
    btn.title = `Subtitles: ${t?.label || "On"} (C)`;
  } else {
    btn.title = "Subtitles: Off (C)";
  }
}

export function updateAudioButtonUI() {
  const btn = byId("ctrlAudioBtn");
  if (!btn) return;
  const showBtn = state.audioTracks.length > 1 || state.needsRemuxHint || state.remuxMode;
  btn.classList.toggle("hidden", !showBtn);
  btn.classList.toggle("active", state.remuxMode);
  btn.classList.toggle("is-on", state.remuxMode);
  if (!showBtn) return;

  const currentTrack =
    state.audioTracks.find((t) => t.index === state.activeAudioIndex) || state.audioTracks[0];
  if (state.audioTracks.length > 1 && currentTrack) {
    const shortLang = (currentTrack.language || `T${currentTrack.index + 1}`)
      .toUpperCase()
      .slice(0, 6);
    btn.textContent = `♪ ${shortLang}`;
    const desc = currentTrack.title || currentTrack.language || `Track ${currentTrack.index + 1}`;
    btn.title = `Audio: ${desc} (${(currentTrack.codec || "").toUpperCase()}) — click to switch`;
  } else if (state.remuxMode) {
    btn.textContent = "♪ Remux";
    btn.title = "Playing via ffmpeg remux — click to return to direct stream";
  } else {
    btn.textContent = "♪ Remux";
    btn.title = "Incompatible audio codec detected — click to remux with ffmpeg";
  }
}

export function clearVideoSubtitles() {
  const video = byId("videoPlayer");
  for (const trackEl of video.querySelectorAll("track")) {
    trackEl.remove();
  }
  state.subtitleTracks = [];
  state.activeSubtitleIndex = -1;
  syncSubtitleTrackModes();
}

export async function loadEpisodeTracks(episodeId, token) {
  try {
    const encodedId = encodePath(episodeId);
    const data = await api(`/api/episodes/${encodedId}/tracks`);
    if (token !== state.tracksRequestToken || state.currentEpisode?.id !== episodeId) return;

    const subtitles = Array.isArray(data.subtitles) ? data.subtitles : [];
    const audio = Array.isArray(data.audio) ? data.audio : [];
    state.subtitleTracks = subtitles;
    state.audioTracks = audio;
    state.needsRemuxHint = Boolean(data.needs_remux_hint);
    if (audio.length > 0 && !audio.some((t) => t.index === state.activeAudioIndex)) {
      state.activeAudioIndex = audio[0].index;
    }

    const video = byId("videoPlayer");
    for (const old of video.querySelectorAll("track")) old.remove();
    for (const t of subtitles) {
      const tr = document.createElement("track");
      tr.kind = "subtitles";
      tr.label = t.label || t.lang || `Track ${t.index + 1}`;
      tr.srclang = t.lang || "und";
      tr.src = `/api/episodes/${encodedId}/subtitles/${encodePath(t.index)}`;
      video.appendChild(tr);
    }

    const pref = readSubtitlePref();
    if (pref.enabled && subtitles.length > 0) {
      let idx = -1;
      if (pref.lang) {
        const wanted = pref.lang.toLowerCase();
        idx = subtitles.findIndex((t) => (t.lang || "").toLowerCase() === wanted);
      }
      state.activeSubtitleIndex = idx >= 0 ? idx : 0;
    } else {
      state.activeSubtitleIndex = -1;
    }

    syncSubtitleTrackModes();
    updateAudioButtonUI();
  } catch {
    // Non-fatal: tracks endpoint failure should not disrupt playback
  }
}

export function cycleSubtitles({ fromKeyboard = false } = {}) {
  if (state.subtitleTracks.length === 0) {
    if (fromKeyboard) showToast("No subtitles available for this episode");
    return;
  }
  if (state.activeSubtitleIndex < 0) {
    state.activeSubtitleIndex = 0;
  } else if (state.activeSubtitleIndex + 1 < state.subtitleTracks.length) {
    state.activeSubtitleIndex += 1;
  } else {
    state.activeSubtitleIndex = -1;
  }

  syncSubtitleTrackModes();
  if (state.activeSubtitleIndex >= 0) {
    const track = state.subtitleTracks[state.activeSubtitleIndex];
    const label = track.label || track.lang || `Track ${track.index + 1}`;
    saveSubtitlePref(true, track.lang || "");
    showToast(`Subtitles: ${label}`);
  } else {
    saveSubtitlePref(false, "");
    showToast("Subtitles: Off");
  }
}

export async function startRemuxStream(ep, startSec = 0, audioIdx = 0, { autoPlay = true } = {}) {
  if (!ep) return false;
  const offset = Math.max(0, Math.floor(Number.isFinite(startSec) ? startSec : 0));
  const url = `/api/episodes/${encodePath(ep.id)}/stream?remux=1&start=${offset}&audio=${encodePath(audioIdx)}`;

  if (!state.ffmpegAvailable) {
    try {
      const health = await api("/api/health");
      if (health?.ffmpeg_available) {
        state.ffmpegAvailable = true;
      }
    } catch {
      // Fall through to preflight check
    }
  }

  if (!state.ffmpegAvailable) {
    const controller = new AbortController();
    try {
      const res = await fetch(url, { signal: controller.signal });
      if (res.status === 501) {
        showToast(
          await parseErrorResponse(res, "Remux unavailable: ffmpeg is not installed on the server.")
        );
        return false;
      }
      controller.abort();
      if (!res.ok) {
        showToast(await parseErrorResponse(res, `Remux failed (${res.status})`));
        return false;
      }
      state.ffmpegAvailable = true;
    } catch (err) {
      if (err?.name !== "AbortError") {
        showToast(`Remux unavailable: ${err.message}`);
        return false;
      }
    }
  }

  const video = byId("videoPlayer");
  state.remuxMode = true;
  state.remuxOffsetSec = offset;
  state.activeAudioIndex = audioIdx;
  state.pendingSeekSec = null;
  state.pendingRemuxSeekSec = null;
  state.autoPlayOnLoad = autoPlay;

  byId("playerErrorBanner").classList.add("hidden");
  video.pause();
  video.src = url;
  video.load();

  updateAudioButtonUI();
  trackCallbacks.onControlsUpdate();
  return true;
}

export async function cycleAudioTrack() {
  const ep = state.currentEpisode;
  if (!ep) return;
  const video = byId("videoPlayer");
  const pos = getEffectiveTime(video);

  if (state.audioTracks.length > 1) {
    const curPos = state.audioTracks.findIndex((t) => t.index === state.activeAudioIndex);
    const nextTrack = state.audioTracks[(curPos + 1) % state.audioTracks.length];
    const ok = await startRemuxStream(ep, pos, nextTrack.index, { autoPlay: true });
    if (ok) {
      const desc = nextTrack.title || nextTrack.language || `Track ${nextTrack.index + 1}`;
      const codec = nextTrack.codec ? ` (${nextTrack.codec.toUpperCase()})` : "";
      showToast(`Audio: ${desc}${codec}`);
    }
    return;
  }

  if (!state.remuxMode) {
    const audioIdx = state.audioTracks[0]?.index ?? 0;
    const ok = await startRemuxStream(ep, pos, audioIdx, { autoPlay: true });
    if (ok) showToast("Remuxing audio stream for browser playback");
  } else {
    state.remuxMode = false;
    state.remuxOffsetSec = 0;
    state.pendingSeekSec = pos;
    state.autoPlayOnLoad = true;
    byId("playerErrorBanner").classList.add("hidden");
    video.pause();
    video.src = `/api/episodes/${encodePath(ep.id)}/stream`;
    video.load();
    updateAudioButtonUI();
    showToast("Switched back to direct stream");
  }
}
