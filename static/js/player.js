import {
  api,
  byId,
  encodePath,
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
  updateSeekIntroHighlight,
  updateSkipIntroVisibility,
} from "./intro.js";
import { renderSeasonSidebar, resetSidebarFilters } from "./sidebar.js";
import {
  clearVideoSubtitles,
  initPlayerTracks,
  loadEpisodeTracks,
  startRemuxStream,
  syncSubtitleTrackModes,
  updateAudioButtonUI,
} from "./player-tracks.js";
import {
  clearControlsTimers,
  exitFullscreenSafe,
  initPlayerControls,
  showControlsTemporarily,
  updatePlayerControlsUI,
  updateTimeAndSeekUI,
  updateTransportStateUI,
} from "./player-controls.js";

export { updatePlayerControlsUI };

let callbacks = {
  onBackToLibrary: async () => {},
  isAnyModalOpen: () => false,
  closeOpenModal: () => false,
};

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
  updateTimeAndSeekUI();
  updateSkipIntroVisibility();
}

export function setView(viewName) {
  state.view = viewName;
  const isLibrary = viewName === "library";
  byId("libraryView").classList.toggle("hidden", !isLibrary);
  byId("watchView").classList.toggle("hidden", isLibrary);
  byId("navLibraryBtn").classList.toggle("hidden", isLibrary);
}

function cancelAutoplayCountdown() {
  if (!state.autoplayTimer && state.autoplayCountdown === 0) return;
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
    const nearEnd =
      canonical.duration_sec > 0 && canonical.position_sec >= canonical.duration_sec * 0.95;
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
  videoPlayer.src = `/api/episodes/${encodePath(canonical.id)}/stream`;
  videoPlayer.load();

  const tracksToken = ++state.tracksRequestToken;
  loadEpisodeTracks(canonical.id, tracksToken);

  refreshIntroUI();
  updatePlayerControlsUI();
  renderSeasonSidebar({ scrollToActive: true });
}

export async function openShow(showId, opts = {}) {
  try {
    const encodedId = encodePath(showId);
    const data = await api(`/api/shows/${encodedId}?autosync=1`);
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
      const nextData = await api(`/api/shows/${encodedId}/next`, { method: "POST", body: "{}" });
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
  const endpoint = `/api/episodes/${encodePath(ep.id)}/progress`;

  if (useBeacon && navigator.sendBeacon) {
    const blob = new Blob([JSON.stringify(payload)], { type: "application/json" });
    navigator.sendBeacon(endpoint, blob);
    return;
  }

  try {
    const res = await api(endpoint, {
      method: "PATCH",
      body: JSON.stringify(payload),
    });
    const updated = res.episode;
    updateEpisodeInState(updated);
    byId("saveStatusPill").textContent = `Saved ${formatTime(updated.position_sec)}`;

    // Live-update active episode progress bar in sidebar without full re-render
    const escapedId =
      typeof CSS !== "undefined" && typeof CSS.escape === "function"
        ? CSS.escape(String(updated.id))
        : String(updated.id);
    const bar = byId("episodeList").querySelector(`[data-ep-progress="${escapedId}"]`);
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
    const res = await api(`/api/shows/${encodePath(state.activeShow.id)}/${direction}`, {
      method: "POST",
      body: "{}",
    });
    setLinkedList(res.linked_list);
    if (!res.episode) {
      showToast(
        isNext ? "You've reached the last episode of this show." : "Already at the first episode."
      );
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
  clearControlsTimers();
  state.introScanToken += 1;
  state.introScanning = false;
  exitFullscreenSafe();
  await saveCurrentProgress({ immediate: true });
  byId("videoPlayer").pause();
  setView("library");
  await callbacks.onBackToLibrary();
}

/* ==================== VIDEO EVENT BINDINGS ==================== */

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
    updateTimeAndSeekUI();

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

  video.addEventListener("progress", updateTimeAndSeekUI);

  video.addEventListener("play", () => {
    updateTransportStateUI();
    showControlsTemporarily();
  });

  video.addEventListener("pause", () => {
    saveCurrentProgress({ immediate: true });
    updateTransportStateUI();
    stage.classList.remove("controls-hidden");
    if (state.controlsHideTimer) {
      clearTimeout(state.controlsHideTimer);
      state.controlsHideTimer = null;
    }
  });

  video.addEventListener("volumechange", updateTransportStateUI);

  video.addEventListener("seeked", () => {
    saveCurrentProgress({ immediate: true });
    updateTimeAndSeekUI();
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

export function initPlayer(hooks = {}) {
  callbacks = { ...callbacks, ...hooks };
  initPlayerTracks({
    onControlsUpdate: updatePlayerControlsUI,
  });
  initPlayerControls({
    seekToTime,
    saveCurrentProgress,
    goNextEpisode,
    goPrevEpisode,
    cancelAutoplayCountdown,
    isAnyModalOpen: () => callbacks.isAnyModalOpen(),
    closeOpenModal: () => callbacks.closeOpenModal(),
  });
  bindVideoEvents();
}
