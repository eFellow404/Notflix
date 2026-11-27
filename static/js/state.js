import { buildEpisodeGroups } from "./seasons.js";

export const state = {
  view: "library",
  shows: [],
  libraryQuery: "",
  librarySort: "recent",
  activeShow: null,
  linkedList: null,
  currentEpisode: null,

  // Season-based episode browser state
  episodeGroups: [],
  activeGroupKey: null,
  episodeQuery: "",
  unwatchedOnly: false,
  reorderMode: false,

  // Playback & controls state
  lastSavedAt: 0,
  upNextDismissedFor: null,
  autoplayNext: true,
  autoplayTimer: null,
  autoplayCountdown: 0,
  browseParent: null,
  browseDirs: [],
  lastAddedParent: "",
  pendingSeekSec: null,
  autoPlayOnLoad: false,
  ffmpegAvailable: false,
  isSeeking: false,
  controlsHideTimer: null,
  clickToggleTimer: null,

  // Intro detection state
  introScanToken: 0,
  introScanning: false,

  // Subtitle, audio track & remux state
  tracksRequestToken: 0,
  subtitleTracks: [],
  activeSubtitleIndex: -1,
  audioTracks: [],
  activeAudioIndex: 0,
  needsRemuxHint: false,
  remuxMode: false,
  remuxOffsetSec: 0,
  pendingRemuxSeekSec: null,

  // Modal targets
  pendingRenameShowId: null,
  pendingDeleteShowId: null,
};

export function getEffectiveTime(video) {
  const base = state.remuxMode ? state.remuxOffsetSec : 0;
  const raw = video && Number.isFinite(video.currentTime) ? video.currentTime : 0;
  return Math.max(0, base + raw);
}

export function getEffectiveDuration(video) {
  const epDur = state.currentEpisode?.duration_sec || 0;
  const vDur = video && Number.isFinite(video.duration) && video.duration > 0 ? video.duration : 0;
  if (state.remuxMode) {
    if (epDur > 0) return epDur;
    return vDur > 0 ? state.remuxOffsetSec + vDur : 0;
  }
  return vDur > 0 ? vDur : epDur;
}

const episodeById = new Map();

export function setLinkedList(linkedList) {
  state.linkedList = linkedList;
  episodeById.clear();
  const episodes = linkedList?.episodes || [];
  for (const ep of episodes) {
    episodeById.set(ep.id, ep);
  }
  state.episodeGroups = buildEpisodeGroups(episodes);
}

export function getEpisodeById(id) {
  if (!id) return null;
  return episodeById.get(id) || null;
}

export function updateEpisodeInState(updated) {
  if (!updated || !updated.id) return null;
  const existing = episodeById.get(updated.id);
  const target = existing || (state.currentEpisode?.id === updated.id ? state.currentEpisode : null);
  if (!target) return null;

  const preservedIndex = target.index;
  const prevId = updated.prev_id !== undefined ? updated.prev_id : target.prev_id;
  const nextId = updated.next_id !== undefined ? updated.next_id : target.next_id;

  Object.assign(target, updated, {
    index: preservedIndex,
    prev_id: prevId,
    next_id: nextId,
  });

  if (state.currentEpisode && state.currentEpisode.id === updated.id) {
    state.currentEpisode = target;
  }
  return target;
}
