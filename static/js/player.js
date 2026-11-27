import {
  ICONS,
  PLAYBACK_SPEEDS,
  api,
  byId,
  formatTime,
  hasValidIntro,
  hasValidOutro,
  showToast,
} from "./utils.js";
import { describeEpisodePosition, findGroupKeyForEpisode } from "./seasons.js";
import {
  getEffectiveDuration,
  getEffectiveTime,
  getEpisodeById,
  setLinkedList,
  state,
  updateEpisodeInState,
} from "./state.js";
import {
  refreshIntroUI,
  runIntroDetection,
  skipIntro,
  updateSeekIntroHighlight,
  updateSkipIntroVisibility,
} from "./intro.js";
import { renderSeasonSidebar, resetSidebarFilters } from "./sidebar.js";

const SUBTITLE_PREF_KEY = "notflix.subtitles";

let callbacks = {
  onBackToLibrary: async () => {},
  isAnyModalOpen: () => false,
  closeOpenModal: () => false,
};

function readSubtitlePref() {
  try {
    const raw = localStorage.getItem(SUBTITLE_PREF_KEY);
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

function saveSubtitlePref(enabled, lang = "") {
  try {
    localStorage.setItem(SUBTITLE_PREF_KEY, JSON.stringify({ enabled: Boolean(enabled), lang: lang || "" }));
  } catch {
    // Ignore storage errors
  }
}

function syncSubtitleTrackModes() {
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

function updateAudioButtonUI() {
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
    const shortLang = (currentTrack.language || `T${currentTrack.index + 1}`).toUpperCase().slice(0, 6);
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

function clearVideoSubtitles() {
  const video = byId("videoPlayer");
  for (const el of video.querySelectorAll("track")) {
    el.remove();
  }
  state.subtitleTracks = [];
  state.activeSubtitleIndex = -1;
  syncSubtitleTrackModes();
}

async function loadEpisodeTracks(episodeId, token) {
  try {
    const data = await api(`/api/episodes/${episodeId}/tracks`);
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
      tr.src = `/api/episodes/${episodeId}/subtitles/${t.index}`;
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

function cycleSubtitles({ fromKeyboard = false } = {}) {
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

async function startRemuxStream(ep, startSec = 0, audioIdx = 0, { autoPlay = true } = {}) {
  if (!ep) return false;
  const offset = Math.max(0, Math.floor(Number.isFinite(startSec) ? startSec : 0));
  const url = `/api/episodes/${ep.id}/stream?remux=1&start=${offset}&audio=${audioIdx}`;

  if (!state.ffmpegAvailable) {
    const controller = new AbortController();
    try {
      const res = await fetch(url, { signal: controller.signal });
      if (res.status === 501) {
        const data = await res.json().catch(() => ({}));
        showToast(data.error || "Remux unavailable: ffmpeg is not installed on the server.");
        return false;
      }
      controller.abort();
      if (!res.ok) {
        const data = await res.json().catch(() => ({}));
        showToast(data.error || `Remux failed (${res.status})`);
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
  updatePlayerControlsUI();
  return true;
}

async function cycleAudioTrack() {
  const ep = state.currentEpisode;
  if (!ep) return;
  const video = byId("videoPlayer");
  const pos = getEffectiveTime(video);

  if (state.audioTracks.length > 1) {
    const curPos = state.audioTracks.findIndex((t) => t.index === state.activeAudioIndex);
    const nextTrack = state.audioTracks[(curPos + 1) % state.audioTracks.length];
    const ok = await startRemuxStream(ep, pos, nextTrack.index, { autoPlay: !video.paused || true });
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
    video.src = `/api/episodes/${ep.id}/stream`;
    video.load();
    updateAudioButtonUI();
    showToast("Switched back to direct stream");
  }
}

export function seekToTime(targetSec, { autoPlay = null } = {}) {
  const video = byId("videoPlayer");
  const dur = getEffectiveDuration(video);
  const clamped = dur > 0 ? Math.max(0, Math.min(dur - 0.25, targetSec)) : Math.max(0, targetSec);
  if (state.remuxMode && state.currentEpisode) {
    const shouldPlay = autoPlay !== null ? autoPlay : !video.paused;
    startRemuxStream(state.currentEpisode, clamped, state.activeAudioIndex, { autoPlay: shouldPlay });
    return;
  }
  video.currentTime = clamped;
  if (autoPlay === true) {
    video.play().catch(() => {});
  }
  updatePlayerControlsUI();
  updateSkipIntroVisibility();
}

function attachEpisodeThumbnails() {
  const list = byId("episodeList");
  if (!list) return;
  for (const card of list.querySelectorAll(".ep-row-card[data-select-ep]")) {
    const epId = card.dataset.selectEp;
    const main = card.querySelector(".ep-row-main");
    if (!epId || !main || main.querySelector(".ep-thumb")) continue;
    const img = document.createElement("img");
    img.className = "ep-thumb";
    img.src = `/api/episodes/${epId}/thumbnail`;
    img.alt = "";
    img.loading = "lazy";
    img.addEventListener("load", () => img.classList.add("loaded"), { once: true });
    img.addEventListener("error", () => img.remove(), { once: true });
    main.insertBefore(img, main.firstChild);
  }
}

function installEpisodeThumbnailObserver() {
  const list = byId("episodeList");
  if (!list || typeof MutationObserver === "undefined") return;
  const observer = new MutationObserver(() => attachEpisodeThumbnails());
  observer.observe(list, { childList: true });
  attachEpisodeThumbnails();
}

export function setView(viewName) {
  state.view = viewName;
  const isLibrary = viewName === "library";
  byId("libraryView").classList.toggle("hidden", !isLibrary);
  byId("watchView").classList.toggle("hidden", isLibrary);
  byId("navLibraryBtn").classList.toggle("hidden", isLibrary);
}

function cancelAutoplayCountdown() {
  if (state.autoplayTimer) {
    clearInterval(state.autoplayTimer);
    state.autoplayTimer = null;
  }
  state.autoplayCountdown = 0;
  byId("upNextLabel").textContent = "UP NEXT";
}

function startAutoplayCountdown() {
  cancelAutoplayCountdown();
  if (!state.autoplayNext || !state.currentEpisode || !state.currentEpisode.next_id) return;
  state.autoplayCountdown = 5;
  byId("upNextLabel").textContent = `UP NEXT IN ${state.autoplayCountdown}s`;
  state.autoplayTimer = setInterval(() => {
    state.autoplayCountdown -= 1;
    if (state.autoplayCountdown <= 0) {
      cancelAutoplayCountdown();
      goNextEpisode();
    } else {
      byId("upNextLabel").textContent = `UP NEXT IN ${state.autoplayCountdown}s`;
    }
  }, 1000);
}

export function refreshCurrentLinksUI() {
  const ep = state.currentEpisode;
  if (!ep || !state.linkedList) return;
  byId("watchPositionLabel").textContent = describeEpisodePosition(
    ep,
    state.linkedList,
    state.episodeGroups
  );
  const prevNode = getEpisodeById(ep.prev_id);
  const nextNode = getEpisodeById(ep.next_id);
  byId("metaAdjacent").textContent = `${prevNode ? prevNode.display_code : "Start"} ← ${ep.display_code} → ${nextNode ? nextNode.display_code : "End"}`;

  const prevEpBtn = byId("prevEpBtn");
  const nextEpBtn = byId("nextEpBtn");
  prevEpBtn.disabled = !prevNode;
  prevEpBtn.textContent = prevNode ? `← ${prevNode.display_code}` : "← Prev";
  nextEpBtn.disabled = !nextNode;
  nextEpBtn.textContent = nextNode ? `Next: ${nextNode.display_code} →` : "End of Show";

  const quickNextBtn = byId("quickNextBtn");
  const ctrlNextEpBtn = byId("ctrlNextEpBtn");
  if (nextNode) {
    byId("quickNextTitle").textContent = nextNode.display_code;
    quickNextBtn.classList.remove("hidden");
    byId("upNextTitle").textContent = `${nextNode.display_code} · ${nextNode.title}`;
    ctrlNextEpBtn.classList.remove("hidden");
  } else {
    quickNextBtn.classList.add("hidden");
    ctrlNextEpBtn.classList.add("hidden");
  }
}

export function loadEpisode(episode, { autoPlay = true, forceStartZero = false } = {}) {
  cancelAutoplayCountdown();
  const canonical = getEpisodeById(episode.id) || episode;
  state.currentEpisode = canonical;
  state.upNextDismissedFor = null;
  state.autoPlayOnLoad = autoPlay;
  state.remuxMode = false;
  state.remuxOffsetSec = 0;
  state.pendingRemuxSeekSec = null;
  state.audioTracks = [];
  state.activeAudioIndex = 0;
  state.needsRemuxHint = false;
  clearVideoSubtitles();
  updateAudioButtonUI();

  // Auto-follow the playing episode's season group
  const targetGroupKey = findGroupKeyForEpisode(state.episodeGroups, canonical.id);
  if (targetGroupKey && state.activeGroupKey !== targetGroupKey) {
    state.activeGroupKey = targetGroupKey;
  }

  byId("playerErrorBanner").classList.add("hidden");
  byId("upNextCard").classList.add("hidden");
  byId("skipIntroOverlay").classList.add("hidden");
  byId("introEditorBar").classList.add("hidden");
  byId("introEditorError").classList.add("hidden");

  let startAt = 0;
  if (!forceStartZero && canonical.position_sec > 1) {
    const nearEnd = canonical.duration_sec > 0 && canonical.position_sec >= canonical.duration_sec * 0.95;
    startAt = nearEnd ? 0 : canonical.position_sec;
  }
  state.pendingSeekSec = startAt;

  byId("watchShowLink").textContent = state.activeShow.name;
  byId("watchPositionLabel").textContent = describeEpisodePosition(
    canonical,
    state.linkedList,
    state.episodeGroups
  );
  byId("watchEpCode").textContent = canonical.display_code;
  byId("watchEpTitle").textContent = canonical.title;
  byId("stageShowTitle").textContent = state.activeShow.name;
  byId("stageEpTitle").textContent = `${canonical.display_code} · ${canonical.title}`;

  const metaRelativePath = byId("metaRelativePath");
  metaRelativePath.textContent = canonical.relative_path;
  metaRelativePath.title = canonical.file_path;
  byId("metaSortReason").textContent = `${canonical.sort_reason} (${Math.round(canonical.confidence * 100)}%)`;

  refreshCurrentLinksUI();

  byId("saveStatusPill").textContent =
    startAt > 1 ? `Resuming ${formatTime(startAt)}` : "Starting at 0:00";

  const videoPlayer = byId("videoPlayer");
  videoPlayer.pause();
  videoPlayer.src = `/api/episodes/${canonical.id}/stream`;
  videoPlayer.load();

  const tracksToken = ++state.tracksRequestToken;
  loadEpisodeTracks(canonical.id, tracksToken);

  refreshIntroUI();
  updatePlayerControlsUI();
  renderSeasonSidebar({ scrollToActive: true });
}

export async function openShow(showId, opts = {}) {
  try {
    const data = await api(`/api/shows/${showId}?autosync=1`);
    state.activeShow = data.show;
    setLinkedList(data.linked_list);
    resetSidebarFilters();
    byId("detailsPanel").classList.add("hidden");
    byId("shortcutsPanel").classList.add("hidden");
    byId("toggleDetailsBtn").setAttribute("aria-expanded", "false");
    byId("toggleShortcutsBtn").setAttribute("aria-expanded", "false");
    setView("watch");

    if (data.diff && (data.diff.added > 0 || data.diff.removed > 0)) {
      showToast(`Folder updated: +${data.diff.added} / -${data.diff.removed} files`, "success");
    }

    if (opts.advanceNextImmediately) {
      const nextData = await api(`/api/shows/${showId}/next`, { method: "POST", body: "{}" });
      setLinkedList(nextData.linked_list);
    }

    const current = getEpisodeById(state.linkedList.current_id) || state.linkedList.episodes[0];
    if (!current) {
      showToast("No playable video files found in this folder.");
      renderSeasonSidebar();
      return;
    }

    state.activeGroupKey = findGroupKeyForEpisode(state.episodeGroups, current.id);
    loadEpisode(current, { autoPlay: Boolean(opts.autoPlay) });

    // Automatically run intro detection once for multi-episode shows not yet scanned
    const eps = state.linkedList.episodes || [];
    const anyIntro = eps.some((ep) => hasValidIntro(ep));
    if (eps.length >= 2 && !state.activeShow.intro_scanned_at && !anyIntro) {
      runIntroDetection(state.activeShow.id, { force: false, silent: true });
    }
  } catch (err) {
    showToast(`Could not open show: ${err.message}`);
  }
}

/* ==================== CUSTOM CINEMA CONTROLS & FULLSCREEN ==================== */

function isStageFullscreen() {
  const stage = byId("playerStage");
  return document.fullscreenElement === stage || document.webkitFullscreenElement === stage;
}

function toggleFullscreen() {
  const stage = byId("playerStage");
  if (document.fullscreenElement || document.webkitFullscreenElement) {
    (document.exitFullscreen || document.webkitExitFullscreen)?.call(document)?.catch?.(() => {});
  } else {
    const req = stage.requestFullscreen || stage.webkitRequestFullscreen;
    req?.call(stage)?.catch?.(() => {});
  }
}

function showControlsTemporarily() {
  const stage = byId("playerStage");
  const video = byId("videoPlayer");
  stage.classList.remove("controls-hidden");
  if (state.controlsHideTimer) {
    clearTimeout(state.controlsHideTimer);
    state.controlsHideTimer = null;
  }
  if (!video.paused && !state.isSeeking) {
    state.controlsHideTimer = setTimeout(() => {
      if (!video.paused && !state.isSeeking) {
        stage.classList.add("controls-hidden");
      }
    }, 2600);
  }
}

function togglePlayPause() {
  const video = byId("videoPlayer");
  if (video.paused) {
    video.play().catch(() => {});
  } else {
    video.pause();
  }
  showControlsTemporarily();
}

function toggleMute() {
  const video = byId("videoPlayer");
  video.muted = !video.muted;
  updatePlayerControlsUI();
  showControlsTemporarily();
}

function cyclePlaybackSpeed() {
  const video = byId("videoPlayer");
  const cur = video.playbackRate || 1;
  const idx = PLAYBACK_SPEEDS.findIndex((s) => Math.abs(s - cur) < 0.01);
  const next = PLAYBACK_SPEEDS[(idx + 1) % PLAYBACK_SPEEDS.length];
  video.playbackRate = next;
  updatePlayerControlsUI();
  showToast(`Playback speed: ${next}×`);
}

export function updatePlayerControlsUI() {
  const v = byId("videoPlayer");
  const dur = getEffectiveDuration(v);
  const cur =
    state.isSeeking && state.remuxMode && state.pendingRemuxSeekSec !== null
      ? state.pendingRemuxSeekSec
      : getEffectiveTime(v);
  const pct = dur > 0 ? Math.max(0, Math.min(100, (cur / dur) * 100)) : 0;

  byId("ctrlPlayBtn").innerHTML = v.paused ? ICONS.play : ICONS.pause;
  byId("ctrlTimeDisplay").textContent = `${formatTime(cur)} / ${formatTime(dur)}`;
  byId("seekProgress").style.width = `${pct}%`;
  byId("seekThumb").style.left = `${pct}%`;
  byId("seekBar").setAttribute("aria-valuenow", String(Math.round(pct)));

  const seekBuffered = byId("seekBuffered");
  if (dur > 0 && v.buffered && v.buffered.length > 0) {
    const bufEnd = (state.remuxMode ? state.remuxOffsetSec : 0) + v.buffered.end(v.buffered.length - 1);
    seekBuffered.style.width = `${Math.max(0, Math.min(100, (bufEnd / dur) * 100))}%`;
  } else {
    seekBuffered.style.width = "0%";
  }

  const effectiveVol = v.muted ? 0 : v.volume;
  byId("ctrlVolumeSlider").value = String(effectiveVol);
  byId("ctrlMuteBtn").innerHTML =
    effectiveVol === 0 ? ICONS.volMute : effectiveVol < 0.5 ? ICONS.volLow : ICONS.volHigh;
  byId("ctrlSpeedBtn").textContent = `${v.playbackRate || 1}×`;
  byId("ctrlAutoplayBtn").classList.toggle("is-on", state.autoplayNext);
  byId("ctrlFullscreenBtn").innerHTML = isStageFullscreen() ? ICONS.fsExit : ICONS.fsEnter;
}

function seekFromClientX(clientX, { commit = true } = {}) {
  const video = byId("videoPlayer");
  const dur = getEffectiveDuration(video);
  if (!Number.isFinite(dur) || dur <= 0) return;
  const rect = byId("seekBar").getBoundingClientRect();
  const ratio = Math.max(0, Math.min(1, (clientX - rect.left) / rect.width));
  const targetSec = ratio * dur;
  if (state.remuxMode) {
    state.pendingRemuxSeekSec = targetSec;
    updatePlayerControlsUI();
    if (commit) {
      const target = state.pendingRemuxSeekSec;
      state.pendingRemuxSeekSec = null;
      seekToTime(target);
    }
    return;
  }
  video.currentTime = targetSec;
  updatePlayerControlsUI();
  updateSkipIntroVisibility();
}

/* ==================== PROGRESS SAVING & NAVIGATION ==================== */

export async function saveCurrentProgress({ immediate = false, watched = null, useBeacon = false } = {}) {
  const ep = state.currentEpisode;
  if (!ep) return;
  const now = Date.now();
  if (!immediate && now - state.lastSavedAt < 4500) return;

  const video = byId("videoPlayer");
  const pos = getEffectiveTime(video);
  const dur = getEffectiveDuration(video);

  const payload = {
    position_sec: Math.max(0, Number.isFinite(pos) ? pos : ep.position_sec),
    duration_sec: dur > 0 ? dur : null,
  };
  if (watched !== null) payload.watched = watched;

  state.lastSavedAt = now;

  if (useBeacon && navigator.sendBeacon) {
    const blob = new Blob([JSON.stringify(payload)], { type: "application/json" });
    navigator.sendBeacon(`/api/episodes/${ep.id}/progress`, blob);
    return;
  }

  try {
    const res = await api(`/api/episodes/${ep.id}/progress`, {
      method: "PATCH",
      body: JSON.stringify(payload),
    });
    const updated = res.episode;
    updateEpisodeInState(updated);
    byId("saveStatusPill").textContent = `Saved ${formatTime(updated.position_sec)}`;

    // Live-update active episode progress bar in sidebar without full re-render
    const bar = byId("episodeList").querySelector(`[data-ep-progress="${updated.id}"]`);
    if (bar) {
      bar.style.width = `${updated.progress_percent}%`;
      bar.classList.toggle("complete", Boolean(updated.watched));
    }
  } catch {
    // Non-fatal during periodic saves
  }
}

async function stepEpisode(direction) {
  if (!state.activeShow) return;
  cancelAutoplayCountdown();
  await saveCurrentProgress({ immediate: true });
  const isNext = direction === "next";
  try {
    const res = await api(`/api/shows/${state.activeShow.id}/${direction}`, {
      method: "POST",
      body: "{}",
    });
    setLinkedList(res.linked_list);
    if (!res.episode) {
      showToast(isNext ? "You've reached the last episode of this show." : "Already at the first episode.");
      return;
    }
    loadEpisode(res.episode, { autoPlay: true });
  } catch (err) {
    showToast(`Could not go to ${isNext ? "next" : "previous"} episode: ${err.message}`);
  }
}

export const goNextEpisode = () => stepEpisode("next");
export const goPrevEpisode = () => stepEpisode("prev");

export async function backToLibrary() {
  if (state.view === "library") return;
  cancelAutoplayCountdown();
  state.introScanToken += 1;
  state.introScanning = false;
  if (document.fullscreenElement || document.webkitFullscreenElement) {
    (document.exitFullscreen || document.webkitExitFullscreen)?.call(document)?.catch?.(() => {});
  }
  await saveCurrentProgress({ immediate: true });
  byId("videoPlayer").pause();
  setView("library");
  await callbacks.onBackToLibrary();
}

/* ==================== EVENT BINDINGS ==================== */

function bindVideoEvents() {
  const video = byId("videoPlayer");
  const spinner = byId("playerSpinner");
  const stage = byId("playerStage");
  const upNextCard = byId("upNextCard");

  video.addEventListener("loadstart", () => spinner.classList.remove("hidden"));
  video.addEventListener("waiting", () => spinner.classList.remove("hidden"));
  video.addEventListener("canplay", () => spinner.classList.add("hidden"));
  video.addEventListener("playing", () => spinner.classList.add("hidden"));

  video.addEventListener("loadedmetadata", () => {
    if (!state.remuxMode && state.pendingSeekSec !== null && state.pendingSeekSec > 0) {
      const maxSeek = Math.max(0, (video.duration || state.pendingSeekSec) - 0.5);
      video.currentTime = Math.min(state.pendingSeekSec, maxSeek);
    }
    state.pendingSeekSec = null;

    syncSubtitleTrackModes();
    updateSeekIntroHighlight();
    updateSkipIntroVisibility();
    updatePlayerControlsUI();

    if (state.autoPlayOnLoad) {
      video.play().catch(() => {
        // Autoplay blocked by browser policy; user can click play
      });
    }
  });

  video.addEventListener("timeupdate", () => {
    saveCurrentProgress({ immediate: false });
    updatePlayerControlsUI();

    const dur = getEffectiveDuration(video);
    const cur = getEffectiveTime(video);
    const ep = state.currentEpisode;
    if (ep && ep.next_id && Number.isFinite(dur) && dur > 0) {
      const hasOutro = hasValidOutro(ep, dur);
      const shouldShowUpNext = hasOutro ? cur >= ep.outro_start_sec : dur > 45 && dur - cur <= 15;
      const shouldHideUpNext = hasOutro ? cur < ep.outro_start_sec - 1 : dur - cur > 16;

      if (shouldShowUpNext && state.upNextDismissedFor !== ep.id) {
        upNextCard.classList.remove("hidden");
      } else if (shouldHideUpNext) {
        upNextCard.classList.add("hidden");
        cancelAutoplayCountdown();
      }
    }
    updateSkipIntroVisibility();
  });

  video.addEventListener("progress", updatePlayerControlsUI);

  video.addEventListener("play", () => {
    updatePlayerControlsUI();
    showControlsTemporarily();
  });

  video.addEventListener("pause", () => {
    saveCurrentProgress({ immediate: true });
    updatePlayerControlsUI();
    stage.classList.remove("controls-hidden");
    if (state.controlsHideTimer) {
      clearTimeout(state.controlsHideTimer);
      state.controlsHideTimer = null;
    }
  });

  video.addEventListener("volumechange", updatePlayerControlsUI);

  video.addEventListener("seeked", () => {
    saveCurrentProgress({ immediate: true });
    updatePlayerControlsUI();
    updateSkipIntroVisibility();
  });

  video.addEventListener("ended", async () => {
    await saveCurrentProgress({ immediate: true, watched: true });
    updatePlayerControlsUI();
    byId("skipIntroOverlay").classList.add("hidden");
    stage.classList.remove("controls-hidden");
    renderSeasonSidebar();
    if (state.currentEpisode && state.currentEpisode.next_id) {
      upNextCard.classList.remove("hidden");
      startAutoplayCountdown();
    }
  });

  video.addEventListener("error", () => {
    spinner.classList.add("hidden");
    const ep = state.currentEpisode;
    if (!ep) return;
    byId("playerErrorBanner").classList.remove("hidden");
    byId("playerErrorPath").textContent = ep.file_path;
    byId("playerErrorMsg").textContent = state.ffmpegAvailable
      ? "Your browser could not natively decode this DVD rip's codec (common with raw .vob, .avi, or AC3 audio). Remuxing to H.264/AAC .mp4 will make it play smoothly in-browser."
      : "Your browser could not natively decode this file's container/codec (common with raw .vob, .avi, or AC3 audio tracks). Convert or remux to H.264/AAC .mp4 (e.g. via HandBrake or ffmpeg) for direct browser playback.";
  });

  window.addEventListener("beforeunload", () => {
    if (state.view === "watch" && state.currentEpisode) {
      saveCurrentProgress({ immediate: true, useBeacon: true });
    }
  });

  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden" && state.view === "watch" && state.currentEpisode) {
      saveCurrentProgress({ immediate: true, useBeacon: true });
    }
  });
}

function bindCinemaControls() {
  const video = byId("videoPlayer");
  const stage = byId("playerStage");
  const seekBar = byId("seekBar");
  const seekHoverTip = byId("seekHoverTip");

  byId("nextEpBtn").addEventListener("click", goNextEpisode);
  byId("quickNextBtn").addEventListener("click", (ev) => {
    ev.stopPropagation();
    goNextEpisode();
  });
  const ctrlNextEpBtn = byId("ctrlNextEpBtn");
  ctrlNextEpBtn.addEventListener("click", () => {
    ctrlNextEpBtn.blur();
    goNextEpisode();
  });
  byId("upNextPlayBtn").addEventListener("click", goNextEpisode);
  byId("prevEpBtn").addEventListener("click", goPrevEpisode);

  byId("upNextDismissBtn").addEventListener("click", () => {
    cancelAutoplayCountdown();
    if (state.currentEpisode) state.upNextDismissedFor = state.currentEpisode.id;
    byId("upNextCard").classList.add("hidden");
    updateSkipIntroVisibility();
  });

  byId("restartEpBtn").addEventListener("click", () => {
    seekToTime(0, { autoPlay: true });
    saveCurrentProgress({ immediate: true });
  });

  byId("btnFixAudioRemux")?.addEventListener("click", async () => {
    const ep = state.currentEpisode;
    if (!ep) return;
    const pos = getEffectiveTime(video) || ep.position_sec || 0;
    const audioIdx = state.audioTracks[0]?.index ?? state.activeAudioIndex ?? 0;
    const ok = await startRemuxStream(ep, pos, audioIdx, { autoPlay: true });
    if (ok) {
      showToast("Reloading episode via ffmpeg remux…");
    }
  });

  video.addEventListener("click", () => {
    if (state.clickToggleTimer) {
      clearTimeout(state.clickToggleTimer);
      state.clickToggleTimer = null;
      return;
    }
    state.clickToggleTimer = setTimeout(() => {
      state.clickToggleTimer = null;
      togglePlayPause();
    }, 180);
  });

  video.addEventListener("dblclick", () => {
    if (state.clickToggleTimer) {
      clearTimeout(state.clickToggleTimer);
      state.clickToggleTimer = null;
    }
    toggleFullscreen();
  });

  stage.addEventListener("mousemove", showControlsTemporarily);
  stage.addEventListener("mouseenter", showControlsTemporarily);
  byId("playerControls").addEventListener("click", (ev) => ev.stopPropagation());

  const bindCtrlBtn = (id, fn) => {
    const btn = byId(id);
    if (!btn) return;
    btn.addEventListener("click", () => {
      btn.blur();
      fn();
    });
  };

  bindCtrlBtn("ctrlPlayBtn", togglePlayPause);
  bindCtrlBtn("ctrlBackBtn", () => {
    seekToTime(Math.max(0, getEffectiveTime(video) - 10));
    showControlsTemporarily();
  });
  bindCtrlBtn("ctrlFwdBtn", () => {
    const dur = getEffectiveDuration(video);
    seekToTime(Math.min(dur || Infinity, getEffectiveTime(video) + 10));
    showControlsTemporarily();
  });
  bindCtrlBtn("ctrlMuteBtn", toggleMute);
  bindCtrlBtn("ctrlSubBtn", () => {
    cycleSubtitles({ fromKeyboard: false });
    showControlsTemporarily();
  });
  bindCtrlBtn("ctrlAudioBtn", () => {
    cycleAudioTrack();
    showControlsTemporarily();
  });
  bindCtrlBtn("ctrlSpeedBtn", cyclePlaybackSpeed);
  bindCtrlBtn("ctrlAutoplayBtn", () => {
    state.autoplayNext = !state.autoplayNext;
    try {
      localStorage.setItem("notflix_autoplay", state.autoplayNext ? "1" : "0");
    } catch {
      // ignore
    }
    updatePlayerControlsUI();
    showToast(`Autoplay next episode: ${state.autoplayNext ? "On" : "Off"}`);
  });
  bindCtrlBtn("ctrlFullscreenBtn", toggleFullscreen);

  const volSlider = byId("ctrlVolumeSlider");
  volSlider.addEventListener("input", () => {
    const vol = Number(volSlider.value);
    video.volume = vol;
    video.muted = vol === 0;
    try {
      localStorage.setItem("notflix_volume", String(vol));
    } catch {
      // ignore
    }
    updatePlayerControlsUI();
  });

  const onFsChange = () => {
    updatePlayerControlsUI();
    showControlsTemporarily();
  };
  document.addEventListener("fullscreenchange", onFsChange);
  document.addEventListener("webkitfullscreenchange", onFsChange);

  // Interactive seek bar with clamped hover tooltip
  seekBar.addEventListener("pointerdown", (ev) => {
    state.isSeeking = true;
    seekBar.classList.add("dragging");
    seekBar.setPointerCapture?.(ev.pointerId);
    seekFromClientX(ev.clientX, { commit: !state.remuxMode });
  });

  seekBar.addEventListener("pointermove", (ev) => {
    const dur = getEffectiveDuration(video);
    const rect = seekBar.getBoundingClientRect();
    const ratio = Math.max(0, Math.min(1, (ev.clientX - rect.left) / rect.width));
    if (Number.isFinite(dur) && dur > 0) {
      const hoverSec = ratio * dur;
      const clampedPx = Math.max(30, Math.min(rect.width - 30, ratio * rect.width));
      seekHoverTip.style.left = `${clampedPx}px`;
      const ep = state.currentEpisode;
      const inHoverIntro =
        hasValidIntro(ep) && hoverSec >= ep.intro_start_sec && hoverSec <= ep.intro_end_sec;
      const inHoverOutro = hasValidOutro(ep, dur) && hoverSec >= ep.outro_start_sec;
      seekHoverTip.textContent = inHoverIntro
        ? `${formatTime(hoverSec)} · Intro`
        : inHoverOutro
        ? `${formatTime(hoverSec)} · Credits`
        : formatTime(hoverSec);
      seekHoverTip.classList.remove("hidden");
    }
    if (state.isSeeking) {
      seekFromClientX(ev.clientX, { commit: false });
    }
  });

  seekBar.addEventListener("pointerup", (ev) => {
    if (!state.isSeeking) return;
    state.isSeeking = false;
    seekBar.classList.remove("dragging");
    seekBar.releasePointerCapture?.(ev.pointerId);
    if (state.remuxMode && state.pendingRemuxSeekSec !== null) {
      const target = state.pendingRemuxSeekSec;
      state.pendingRemuxSeekSec = null;
      seekToTime(target);
    }
    saveCurrentProgress({ immediate: true });
    showControlsTemporarily();
  });

  seekBar.addEventListener("pointerleave", () => {
    if (!state.isSeeking) {
      seekHoverTip.classList.add("hidden");
    }
  });

  // Utility strip disclosure panels
  const toggleDetailsBtn = byId("toggleDetailsBtn");
  toggleDetailsBtn.addEventListener("click", () => {
    const open = byId("detailsPanel").classList.toggle("hidden") === false;
    toggleDetailsBtn.setAttribute("aria-expanded", String(open));
  });

  const toggleShortcutsBtn = byId("toggleShortcutsBtn");
  toggleShortcutsBtn.addEventListener("click", () => {
    const open = byId("shortcutsPanel").classList.toggle("hidden") === false;
    toggleShortcutsBtn.setAttribute("aria-expanded", String(open));
  });
}

function bindKeyboardShortcuts() {
  const video = byId("videoPlayer");
  const stage = byId("playerStage");

  document.addEventListener("keydown", (ev) => {
    if (ev.key === "Escape") {
      if (callbacks.closeOpenModal()) return;
      const introEditorBar = byId("introEditorBar");
      if (!introEditorBar.classList.contains("hidden")) {
        introEditorBar.classList.add("hidden");
        return;
      }
    }

    if (state.view !== "watch" || callbacks.isAnyModalOpen()) return;
    const tag = (ev.target && ev.target.tagName) || "";
    if (["INPUT", "TEXTAREA", "SELECT"].includes(tag)) return;
    if (tag === "BUTTON" && ev.code !== "Space") return;

    if (ev.code === "Space" || ev.key === "k" || ev.key === "K") {
      if (tag === "BUTTON" && !stage.contains(ev.target)) return;
      ev.preventDefault();
      togglePlayPause();
    } else if (ev.key === "ArrowRight") {
      ev.preventDefault();
      const dur = getEffectiveDuration(video);
      seekToTime(Math.min(dur || Infinity, getEffectiveTime(video) + 10));
      showControlsTemporarily();
    } else if (ev.key === "ArrowLeft") {
      ev.preventDefault();
      seekToTime(Math.max(0, getEffectiveTime(video) - 10));
      showControlsTemporarily();
    } else if (ev.key === "ArrowUp") {
      ev.preventDefault();
      video.muted = false;
      video.volume = Math.min(1, Math.round((video.volume + 0.1) * 100) / 100);
      showControlsTemporarily();
    } else if (ev.key === "ArrowDown") {
      ev.preventDefault();
      video.volume = Math.max(0, Math.round((video.volume - 0.1) * 100) / 100);
      showControlsTemporarily();
    } else if (ev.key === "s" || ev.key === "S") {
      if (!byId("skipIntroOverlay").classList.contains("hidden")) {
        ev.preventDefault();
        skipIntro();
      }
    } else if (ev.key === "c" || ev.key === "C") {
      ev.preventDefault();
      cycleSubtitles({ fromKeyboard: true });
      showControlsTemporarily();
    } else if (ev.key === "m" || ev.key === "M") {
      ev.preventDefault();
      toggleMute();
    } else if (ev.key === "n" || ev.key === "N") {
      ev.preventDefault();
      goNextEpisode();
    } else if (ev.key === "p" || ev.key === "P") {
      ev.preventDefault();
      goPrevEpisode();
    } else if (ev.key === "r" || ev.key === "R") {
      ev.preventDefault();
      seekToTime(0);
      saveCurrentProgress({ immediate: true });
    } else if (ev.key === "f" || ev.key === "F") {
      ev.preventDefault();
      toggleFullscreen();
    }
  });
}

export function initPlayer(hooks = {}) {
  callbacks = { ...callbacks, ...hooks };
  bindVideoEvents();
  bindCinemaControls();
  bindKeyboardShortcuts();
  installEpisodeThumbnailObserver();
}
