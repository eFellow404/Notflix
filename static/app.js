import { api, byId, storageGet } from "./js/utils.js";
import { state } from "./js/state.js";
import { initIntro } from "./js/intro.js";
import { initSidebar, renderSeasonSidebar } from "./js/sidebar.js";
import { initUpdater } from "./js/updater.js";
import {
  backToLibrary,
  initPlayer,
  loadEpisode,
  openShow,
  refreshCurrentLinksUI,
  saveCurrentProgress,
  seekToTime,
  updatePlayerControlsUI,
} from "./js/player.js";
import {
  closeOpenModal,
  initLibrary,
  isAnyModalOpen,
  loadLibrary,
  openAddModal,
} from "./js/library.js";

function wireModules() {
  initIntro({
    onIntroUpdated: () => renderSeasonSidebar(),
    onSeekTo: (targetSec, opts) => seekToTime(targetSec, opts),
    onAfterSkipIntro: () => {
      updatePlayerControlsUI();
      saveCurrentProgress({ immediate: true });
    },
  });

  initSidebar({
    onBeforeEpisodeSwitch: () => saveCurrentProgress({ immediate: true }),
    onEpisodeSelected: (episode) => loadEpisode(episode, { autoPlay: true }),
    onLinksChanged: () => refreshCurrentLinksUI(),
  });

  initPlayer({
    onBackToLibrary: () => loadLibrary(),
    isAnyModalOpen,
    closeOpenModal,
  });

  initLibrary({
    onOpenShow: (showId, opts) => openShow(showId, opts),
  });

  initUpdater();

  // Top-level navigation & add-show triggers
  byId("brandBtn").addEventListener("click", backToLibrary);
  byId("navLibraryBtn").addEventListener("click", backToLibrary);
  byId("watchShowLink").addEventListener("click", backToLibrary);
  byId("openAddModalBtn").addEventListener("click", openAddModal);
  byId("emptyAddBtn").addEventListener("click", openAddModal);
}

function restorePreferences() {
  const savedVol = storageGet("notflix_volume");
  if (savedVol !== null && Number.isFinite(Number(savedVol))) {
    byId("videoPlayer").volume = Math.max(0, Math.min(1, Number(savedVol)));
  }
  const savedAuto = storageGet("notflix_autoplay");
  if (savedAuto !== null) {
    state.autoplayNext = savedAuto !== "0";
  }
}

async function init() {
  wireModules();
  restorePreferences();
  updatePlayerControlsUI();

  try {
    const health = await api("/api/health");
    state.ffmpegAvailable = Boolean(health.ffmpeg_available);
  } catch {
    // Ignore transient health check failure
  }

  await loadLibrary();
}

init();
