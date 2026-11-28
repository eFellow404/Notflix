import {
  ICONS,
  PLAYBACK_SPEEDS,
  byId,
  formatTime,
  hasValidIntro,
  hasValidOutro,
  showToast,
  storageSet,
} from "./utils.js";
import { getEffectiveDuration, getEffectiveTime, state } from "./state.js";
import { skipIntro, updateSkipIntroVisibility } from "./intro.js";
import { cycleAudioTrack, cycleSubtitles, startRemuxStream } from "./player-tracks.js";

let controlHooks = {
  seekToTime: () => {},
  saveCurrentProgress: async () => {},
  goNextEpisode: () => {},
  goPrevEpisode: () => {},
  cancelAutoplayCountdown: () => {},
  isAnyModalOpen: () => false,
  closeOpenModal: () => false,
};

let dom = null;
let lastControlsShowAt = 0;
let lastTimeText = "";
let lastAriaVal = -1;
let lastBufferedWidth = "";
let lastPlayIconKey = "";
let lastMuteIconKey = "";
let lastFsIconKey = "";
let lastSpeedText = "";

function getDom() {
  if (!dom) {
    dom = {
      video: byId("videoPlayer"),
      stage: byId("playerStage"),
      seekBar: byId("seekBar"),
      seekBuffered: byId("seekBuffered"),
      seekProgress: byId("seekProgress"),
      seekThumb: byId("seekThumb"),
      seekHoverTip: byId("seekHoverTip"),
      ctrlPlayBtn: byId("ctrlPlayBtn"),
      ctrlTimeDisplay: byId("ctrlTimeDisplay"),
      ctrlVolumeSlider: byId("ctrlVolumeSlider"),
      ctrlMuteBtn: byId("ctrlMuteBtn"),
      ctrlSpeedBtn: byId("ctrlSpeedBtn"),
      ctrlAutoplayBtn: byId("ctrlAutoplayBtn"),
      ctrlFullscreenBtn: byId("ctrlFullscreenBtn"),
      ctrlNextEpBtn: byId("ctrlNextEpBtn"),
      playerControls: byId("playerControls"),
      upNextCard: byId("upNextCard"),
      skipIntroOverlay: byId("skipIntroOverlay"),
      introEditorBar: byId("introEditorBar"),
      detailsPanel: byId("detailsPanel"),
      shortcutsPanel: byId("shortcutsPanel"),
      toggleDetailsBtn: byId("toggleDetailsBtn"),
      toggleShortcutsBtn: byId("toggleShortcutsBtn"),
    };
  }
  return dom;
}

export function getFullscreenElement() {
  return document.fullscreenElement || document.webkitFullscreenElement || null;
}

export function exitFullscreenSafe() {
  if (!getFullscreenElement()) return;
  (document.exitFullscreen || document.webkitExitFullscreen)?.call(document)?.catch?.(() => {});
}

export function isStageFullscreen() {
  return getFullscreenElement() === getDom().stage;
}

function toggleFullscreen() {
  const { stage } = getDom();
  if (getFullscreenElement()) {
    exitFullscreenSafe();
  } else {
    const req = stage.requestFullscreen || stage.webkitRequestFullscreen;
    req?.call(stage)?.catch?.(() => {});
  }
}

export function clearControlsTimers() {
  if (state.controlsHideTimer) {
    clearTimeout(state.controlsHideTimer);
    state.controlsHideTimer = null;
  }
  if (state.clickToggleTimer) {
    clearTimeout(state.clickToggleTimer);
    state.clickToggleTimer = null;
  }
}

export function showControlsTemporarily({ fromMouseMove = false } = {}) {
  const { stage, video } = getDom();
  const now = Date.now();
  if (fromMouseMove && !stage.classList.contains("controls-hidden") && now - lastControlsShowAt < 150) {
    return;
  }
  lastControlsShowAt = now;
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
  const { video } = getDom();
  if (video.paused) {
    video.play().catch(() => {});
  } else {
    video.pause();
  }
  showControlsTemporarily();
}

function toggleMute() {
  const { video } = getDom();
  video.muted = !video.muted;
  updateTransportStateUI();
  showControlsTemporarily();
}

function cyclePlaybackSpeed() {
  const { video } = getDom();
  const cur = video.playbackRate || 1;
  const idx = PLAYBACK_SPEEDS.findIndex((s) => Math.abs(s - cur) < 0.01);
  const next = PLAYBACK_SPEEDS[(idx + 1) % PLAYBACK_SPEEDS.length];
  video.playbackRate = next;
  updateTransportStateUI();
  showToast(`Playback speed: ${next}×`);
}

// Hot-path UI update (timeupdate / progress / scrubbing): ZERO .innerHTML writes
export function updateTimeAndSeekUI() {
  const { video: v, ctrlTimeDisplay, seekProgress, seekThumb, seekBar, seekBuffered } = getDom();
  const dur = getEffectiveDuration(v);
  const cur =
    state.isSeeking && state.remuxMode && state.pendingRemuxSeekSec !== null
      ? state.pendingRemuxSeekSec
      : getEffectiveTime(v);
  const pct = dur > 0 ? Math.max(0, Math.min(100, (cur / dur) * 100)) : 0;

  const timeText = `${formatTime(cur)} / ${formatTime(dur)}`;
  if (timeText !== lastTimeText) {
    lastTimeText = timeText;
    ctrlTimeDisplay.textContent = timeText;
  }

  seekProgress.style.width = `${pct}%`;
  seekThumb.style.left = `${pct}%`;

  const roundedPct = Math.round(pct);
  if (roundedPct !== lastAriaVal) {
    lastAriaVal = roundedPct;
    seekBar.setAttribute("aria-valuenow", String(roundedPct));
  }

  let bufWidth = "0%";
  if (dur > 0 && v.buffered && v.buffered.length > 0) {
    const bufEnd =
      (state.remuxMode ? state.remuxOffsetSec : 0) + v.buffered.end(v.buffered.length - 1);
    bufWidth = `${Math.max(0, Math.min(100, (bufEnd / dur) * 100))}%`;
  }
  if (bufWidth !== lastBufferedWidth) {
    lastBufferedWidth = bufWidth;
    seekBuffered.style.width = bufWidth;
  }
}

// Event-driven UI update: only writes SVG icons on actual state transitions
export function updateTransportStateUI() {
  const {
    video: v,
    ctrlPlayBtn,
    ctrlVolumeSlider,
    ctrlMuteBtn,
    ctrlSpeedBtn,
    ctrlAutoplayBtn,
    ctrlFullscreenBtn,
  } = getDom();

  const playKey = v.paused ? "play" : "pause";
  if (playKey !== lastPlayIconKey) {
    lastPlayIconKey = playKey;
    ctrlPlayBtn.innerHTML = ICONS[playKey];
  }

  const effectiveVol = v.muted ? 0 : v.volume;
  ctrlVolumeSlider.value = String(effectiveVol);

  const muteKey = effectiveVol === 0 ? "volMute" : effectiveVol < 0.5 ? "volLow" : "volHigh";
  if (muteKey !== lastMuteIconKey) {
    lastMuteIconKey = muteKey;
    ctrlMuteBtn.innerHTML = ICONS[muteKey];
  }

  const speedText = `${v.playbackRate || 1}×`;
  if (speedText !== lastSpeedText) {
    lastSpeedText = speedText;
    ctrlSpeedBtn.textContent = speedText;
  }

  ctrlAutoplayBtn.classList.toggle("is-on", state.autoplayNext);

  const fsKey = isStageFullscreen() ? "fsExit" : "fsEnter";
  if (fsKey !== lastFsIconKey) {
    lastFsIconKey = fsKey;
    ctrlFullscreenBtn.innerHTML = ICONS[fsKey];
  }
}

export function updatePlayerControlsUI() {
  updateTimeAndSeekUI();
  updateTransportStateUI();
}

function seekFromRatio(ratio, { commit = true } = {}) {
  const { video } = getDom();
  const dur = getEffectiveDuration(video);
  if (!Number.isFinite(dur) || dur <= 0) return;
  const clampedRatio = Math.max(0, Math.min(1, ratio));
  const targetSec = clampedRatio * dur;
  if (state.remuxMode) {
    state.pendingRemuxSeekSec = targetSec;
    updateTimeAndSeekUI();
    if (commit) {
      const target = state.pendingRemuxSeekSec;
      state.pendingRemuxSeekSec = null;
      controlHooks.seekToTime(target);
    }
    return;
  }
  video.currentTime = targetSec;
  updateTimeAndSeekUI();
  updateSkipIntroVisibility();
}

function seekFromClientX(clientX, { commit = true, rect = null } = {}) {
  const barRect = rect || getDom().seekBar.getBoundingClientRect();
  if (!barRect.width) return;
  const ratio = (clientX - barRect.left) / barRect.width;
  seekFromRatio(ratio, { commit });
}

function bindCinemaControls() {
  const {
    video,
    stage,
    seekBar,
    seekHoverTip,
    ctrlNextEpBtn,
    ctrlVolumeSlider,
    playerControls,
    upNextCard,
    detailsPanel,
    shortcutsPanel,
    toggleDetailsBtn,
    toggleShortcutsBtn,
  } = getDom();

  byId("nextEpBtn").addEventListener("click", () => controlHooks.goNextEpisode());
  byId("quickNextBtn").addEventListener("click", (ev) => {
    ev.stopPropagation();
    controlHooks.goNextEpisode();
  });
  ctrlNextEpBtn.addEventListener("click", () => {
    ctrlNextEpBtn.blur();
    controlHooks.goNextEpisode();
  });
  byId("upNextPlayBtn").addEventListener("click", () => controlHooks.goNextEpisode());
  byId("prevEpBtn").addEventListener("click", () => controlHooks.goPrevEpisode());

  byId("upNextDismissBtn").addEventListener("click", () => {
    controlHooks.cancelAutoplayCountdown();
    if (state.currentEpisode) state.upNextDismissedFor = state.currentEpisode.id;
    upNextCard.classList.add("hidden");
    updateSkipIntroVisibility();
  });

  byId("restartEpBtn").addEventListener("click", () => {
    controlHooks.seekToTime(0, { autoPlay: true });
    controlHooks.saveCurrentProgress({ immediate: true });
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

  stage.addEventListener("mousemove", () => showControlsTemporarily({ fromMouseMove: true }));
  stage.addEventListener("mouseenter", () => showControlsTemporarily());
  playerControls.addEventListener("click", (ev) => ev.stopPropagation());

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
    controlHooks.seekToTime(Math.max(0, getEffectiveTime(video) - 10));
    showControlsTemporarily();
  });
  bindCtrlBtn("ctrlFwdBtn", () => {
    const dur = getEffectiveDuration(video);
    controlHooks.seekToTime(Math.min(dur || Infinity, getEffectiveTime(video) + 10));
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
    storageSet("notflix_autoplay", state.autoplayNext ? "1" : "0");
    updateTransportStateUI();
    showToast(`Autoplay next episode: ${state.autoplayNext ? "On" : "Off"}`);
  });
  bindCtrlBtn("ctrlFullscreenBtn", toggleFullscreen);

  ctrlVolumeSlider.addEventListener("input", () => {
    const vol = Number(ctrlVolumeSlider.value);
    video.volume = vol;
    video.muted = vol === 0;
    storageSet("notflix_volume", String(vol));
    updateTransportStateUI();
  });

  const onFsChange = () => {
    updateTransportStateUI();
    showControlsTemporarily();
  };
  document.addEventListener("fullscreenchange", onFsChange);
  document.addEventListener("webkitfullscreenchange", onFsChange);

  // Interactive seek bar with clamped hover tooltip (single layout read per move)
  seekBar.addEventListener("pointerdown", (ev) => {
    state.isSeeking = true;
    seekBar.classList.add("dragging");
    seekBar.setPointerCapture?.(ev.pointerId);
    seekFromClientX(ev.clientX, { commit: !state.remuxMode });
  });

  seekBar.addEventListener("pointermove", (ev) => {
    const dur = getEffectiveDuration(video);
    const rect = seekBar.getBoundingClientRect();
    const ratio = rect.width > 0 ? Math.max(0, Math.min(1, (ev.clientX - rect.left) / rect.width)) : 0;
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
      seekFromRatio(ratio, { commit: false });
    }
  });

  const finishSeek = (ev, { commit }) => {
    if (!state.isSeeking) return;
    state.isSeeking = false;
    seekBar.classList.remove("dragging");
    if (seekBar.hasPointerCapture?.(ev.pointerId)) {
      seekBar.releasePointerCapture?.(ev.pointerId);
    }
    if (commit) {
      if (state.remuxMode && state.pendingRemuxSeekSec !== null) {
        const target = state.pendingRemuxSeekSec;
        state.pendingRemuxSeekSec = null;
        controlHooks.seekToTime(target);
      }
      controlHooks.saveCurrentProgress({ immediate: true });
    } else {
      state.pendingRemuxSeekSec = null;
      seekHoverTip.classList.add("hidden");
      updateTimeAndSeekUI();
    }
    showControlsTemporarily();
  };

  seekBar.addEventListener("pointerup", (ev) => finishSeek(ev, { commit: true }));
  seekBar.addEventListener("pointercancel", (ev) => finishSeek(ev, { commit: false }));

  seekBar.addEventListener("pointerleave", () => {
    if (!state.isSeeking) {
      seekHoverTip.classList.add("hidden");
    }
  });

  // Utility strip disclosure panels
  toggleDetailsBtn.addEventListener("click", () => {
    const open = detailsPanel.classList.toggle("hidden") === false;
    toggleDetailsBtn.setAttribute("aria-expanded", String(open));
  });

  toggleShortcutsBtn.addEventListener("click", () => {
    const open = shortcutsPanel.classList.toggle("hidden") === false;
    toggleShortcutsBtn.setAttribute("aria-expanded", String(open));
  });
}

function bindKeyboardShortcuts() {
  const { video, stage, skipIntroOverlay, introEditorBar } = getDom();

  document.addEventListener("keydown", (ev) => {
    if (ev.key === "Escape") {
      if (controlHooks.closeOpenModal()) return;
      if (!introEditorBar.classList.contains("hidden")) {
        introEditorBar.classList.add("hidden");
        return;
      }
    }

    if (state.view !== "watch" || controlHooks.isAnyModalOpen()) return;
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
      controlHooks.seekToTime(Math.min(dur || Infinity, getEffectiveTime(video) + 10));
      showControlsTemporarily();
    } else if (ev.key === "ArrowLeft") {
      ev.preventDefault();
      controlHooks.seekToTime(Math.max(0, getEffectiveTime(video) - 10));
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
      if (!skipIntroOverlay.classList.contains("hidden")) {
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
      controlHooks.goNextEpisode();
    } else if (ev.key === "p" || ev.key === "P") {
      ev.preventDefault();
      controlHooks.goPrevEpisode();
    } else if (ev.key === "r" || ev.key === "R") {
      ev.preventDefault();
      controlHooks.seekToTime(0);
      controlHooks.saveCurrentProgress({ immediate: true });
    } else if (ev.key === "f" || ev.key === "F") {
      ev.preventDefault();
      toggleFullscreen();
    }
  });
}

export function initPlayerControls(hooks = {}) {
  controlHooks = { ...controlHooks, ...hooks };
  getDom();
  bindCinemaControls();
  bindKeyboardShortcuts();
}
