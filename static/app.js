const state = {
  view: "library",
  shows: [],
  activeShow: null,
  linkedList: null,
  currentEpisode: null,
  lastSavedAt: 0,
  upNextDismissedFor: null,
  browseParent: null,
  pendingSeekSec: null,
  autoPlayOnLoad: false,
  ffmpegAvailable: false,
};

const els = {
  brandBtn: document.getElementById("brandBtn"),
  navLibraryBtn: document.getElementById("navLibraryBtn"),
  openAddModalBtn: document.getElementById("openAddModalBtn"),
  emptyAddBtn: document.getElementById("emptyAddBtn"),

  libraryView: document.getElementById("libraryView"),
  continueHero: document.getElementById("continueHero"),
  showsGrid: document.getElementById("showsGrid"),
  emptyLibrary: document.getElementById("emptyLibrary"),

  watchView: document.getElementById("watchView"),
  watchShowName: document.getElementById("watchShowName"),
  watchChainPos: document.getElementById("watchChainPos"),
  watchEpCode: document.getElementById("watchEpCode"),
  watchEpTitle: document.getElementById("watchEpTitle"),
  saveStatusPill: document.getElementById("saveStatusPill"),
  prevEpBtn: document.getElementById("prevEpBtn"),
  restartEpBtn: document.getElementById("restartEpBtn"),
  nextEpBtn: document.getElementById("nextEpBtn"),

  videoPlayer: document.getElementById("videoPlayer"),
  quickNextOverlay: document.getElementById("quickNextOverlay"),
  quickNextBtn: document.getElementById("quickNextBtn"),
  quickNextTitle: document.getElementById("quickNextTitle"),
  upNextCard: document.getElementById("upNextCard"),
  upNextTitle: document.getElementById("upNextTitle"),
  upNextPlayBtn: document.getElementById("upNextPlayBtn"),
  upNextDismissBtn: document.getElementById("upNextDismissBtn"),
  playerErrorBanner: document.getElementById("playerErrorBanner"),
  playerErrorMsg: document.getElementById("playerErrorMsg"),
  playerErrorPath: document.getElementById("playerErrorPath"),

  metaRelativePath: document.getElementById("metaRelativePath"),
  metaSortReason: document.getElementById("metaSortReason"),
  metaChainLinks: document.getElementById("metaChainLinks"),

  chainSummary: document.getElementById("chainSummary"),
  rescanShowBtn: document.getElementById("rescanShowBtn"),
  resetSmartSortBtn: document.getElementById("resetSmartSortBtn"),
  episodeList: document.getElementById("episodeList"),

  addModalBackdrop: document.getElementById("addModalBackdrop"),
  closeAddModalBtn: document.getElementById("closeAddModalBtn"),
  cancelAddModalBtn: document.getElementById("cancelAddModalBtn"),
  confirmAddShowBtn: document.getElementById("confirmAddShowBtn"),
  showNameInput: document.getElementById("showNameInput"),
  folderPathInput: document.getElementById("folderPathInput"),
  browseGoBtn: document.getElementById("browseGoBtn"),
  browseUpBtn: document.getElementById("browseUpBtn"),
  browserCurrentPath: document.getElementById("browserCurrentPath"),
  browserVideoBadge: document.getElementById("browserVideoBadge"),
  browserList: document.getElementById("browserList"),
  addModalError: document.getElementById("addModalError"),

  toastContainer: document.getElementById("toastContainer"),
};

function formatTime(sec) {
  if (!Number.isFinite(sec) || sec < 0) return "0:00";
  const total = Math.floor(sec);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  if (h > 0) {
    return `${h}:${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}`;
  }
  return `${m}:${String(s).padStart(2, "0")}`;
}

function escapeHtml(str) {
  return String(str ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function showToast(message) {
  const el = document.createElement("div");
  el.className = "toast";
  el.textContent = message;
  els.toastContainer.appendChild(el);
  setTimeout(() => el.remove(), 3400);
}

async function api(path, options = {}) {
  const init = {
    headers: { "Content-Type": "application/json", ...(options.headers || {}) },
    ...options,
  };
  const res = await fetch(path, init);
  const data = await res.json().catch(() => ({}));
  if (!res.ok) {
    throw new Error(data.error || `Request failed (${res.status})`);
  }
  return data;
}

function getEpisodeById(id) {
  if (!state.linkedList || !id) return null;
  return state.linkedList.episodes.find((e) => e.id === id) || null;
}

function setView(viewName) {
  state.view = viewName;
  if (viewName === "library") {
    els.libraryView.classList.remove("hidden");
    els.watchView.classList.add("hidden");
    els.navLibraryBtn.classList.add("hidden");
  } else {
    els.libraryView.classList.add("hidden");
    els.watchView.classList.remove("hidden");
    els.navLibraryBtn.classList.remove("hidden");
  }
}

/* ==================== LIBRARY ==================== */

async function loadLibrary() {
  try {
    const data = await api("/api/shows");
    state.shows = data.shows || [];
    renderLibrary();
  } catch (err) {
    showToast(`Failed to load library: ${err.message}`);
  }
}

function renderLibrary() {
  const shows = state.shows;
  if (shows.length === 0) {
    els.continueHero.classList.add("hidden");
    els.showsGrid.innerHTML = "";
    els.emptyLibrary.classList.remove("hidden");
    return;
  }

  els.emptyLibrary.classList.add("hidden");

  // Continue Watching hero picks the most recently watched show (or first show with episodes)
  const heroShow = shows.find((s) => s.current_episode) || shows[0];
  if (heroShow && heroShow.current_episode) {
    const ep = heroShow.current_episode;
    const nxt = heroShow.next_episode;
    const timeLabel =
      ep.position_sec > 2
        ? `Saved at ${formatTime(ep.position_sec)}${ep.duration_sec > 0 ? ` / ${formatTime(ep.duration_sec)}` : ""}`
        : "Ready to start";

    els.continueHero.classList.remove("hidden");
    els.continueHero.innerHTML = `
      <div>
        <div class="hero-eyebrow">${heroShow.last_watched_at ? "Continue Watching" : "Ready To Watch"}</div>
        <h2>${escapeHtml(heroShow.name)}</h2>
        <div class="hero-ep-line">
          <span class="ep-code-badge">${escapeHtml(ep.display_code)}</span>
          <strong>${escapeHtml(ep.title)}</strong>
          <span class="muted">· ${escapeHtml(timeLabel)}</span>
        </div>
        <div class="hero-progress-wrap">
          <div class="progress-track">
            <div class="progress-fill" style="width: ${ep.progress_percent}%"></div>
          </div>
        </div>
      </div>
      <div class="hero-actions">
        <button class="btn btn-primary btn-lg" data-hero-watch="${heroShow.id}" type="button">
          ▶ ${ep.position_sec > 2 ? `Resume (${formatTime(ep.position_sec)})` : "Watch Now"}
        </button>
        ${
          nxt
            ? `<button class="btn btn-secondary btn-lg" data-hero-next="${heroShow.id}" type="button">
                Next: ${escapeHtml(nxt.display_code)} →
              </button>`
            : ""
        }
      </div>
    `;

    els.continueHero.querySelector("[data-hero-watch]")?.addEventListener("click", () => {
      openShow(heroShow.id, { autoPlay: true });
    });
    els.continueHero.querySelector("[data-hero-next]")?.addEventListener("click", async () => {
      await openShow(heroShow.id, { advanceNextImmediately: true, autoPlay: true });
    });
  } else {
    els.continueHero.classList.add("hidden");
  }

  els.showsGrid.innerHTML = shows
    .map((show) => {
      const ep = show.current_episode;
      const pctShow =
        show.episode_count > 0
          ? Math.round((show.watched_count / show.episode_count) * 100)
          : 0;
      const epInfoHtml = ep
        ? `
          <div class="show-current-box">
            <div class="show-current-row">
              <span><strong>${escapeHtml(ep.display_code)}</strong> · ${escapeHtml(ep.title)}</span>
              <span class="muted">${ep.position_sec > 2 ? formatTime(ep.position_sec) : "0:00"}</span>
            </div>
            <div class="progress-track">
              <div class="progress-fill" style="width: ${ep.progress_percent}%"></div>
            </div>
          </div>
        `
        : `<div class="show-current-box muted">No video files found in this folder yet.</div>`;

      return `
        <article class="show-card">
          <div class="show-card-top">
            <div>
              <h3 class="show-card-title" data-open-show="${show.id}">${escapeHtml(show.name)}</h3>
              <div class="show-card-path">${escapeHtml(show.folder_path)}</div>
            </div>
          </div>
          <div class="show-stats">
            <span class="pill">${show.episode_count} episode${show.episode_count === 1 ? "" : "s"}</span>
            <span class="pill">${show.watched_count}/${show.episode_count} watched (${pctShow}%)</span>
            ${show.manual_order ? `<span class="pill">Manual Order</span>` : `<span class="pill">Smart Ordered</span>`}
            ${!show.folder_exists ? `<span class="pill pill-warn">Folder Missing</span>` : ""}
          </div>
          ${epInfoHtml}
          <div class="show-card-actions">
            <button class="btn btn-primary btn-sm" data-open-show="${show.id}" type="button" ${show.episode_count === 0 ? "disabled" : ""}>
              ▶ ${ep && ep.position_sec > 2 ? "Resume" : "Watch"}
            </button>
            <button class="btn btn-secondary btn-sm" data-rescan-show="${show.id}" type="button" title="Rescan folder for new files">
              ↻ Rescan
            </button>
            <button class="btn btn-danger btn-sm" data-delete-show="${show.id}" type="button" title="Remove from Notflix (keeps files on disk)">
              ✕
            </button>
          </div>
        </article>
      `;
    })
    .join("");

  els.showsGrid.querySelectorAll("[data-open-show]").forEach((btn) => {
    btn.addEventListener("click", () => openShow(btn.dataset.openShow, { autoPlay: true }));
  });
  els.showsGrid.querySelectorAll("[data-rescan-show]").forEach((btn) => {
    btn.addEventListener("click", async () => {
      try {
        const res = await api(`/api/shows/${btn.dataset.rescanShow}/rescan`, {
          method: "POST",
          body: JSON.stringify({ reset_order: false }),
        });
        showToast(
          `Rescanned: ${res.diff.added} added, ${res.diff.removed} removed (${res.diff.total} total)`
        );
        await loadLibrary();
      } catch (err) {
        showToast(`Rescan failed: ${err.message}`);
      }
    });
  });
  els.showsGrid.querySelectorAll("[data-delete-show]").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const show = state.shows.find((s) => s.id === btn.dataset.deleteShow);
      if (!confirm(`Remove "${show?.name || "this show"}" from Notflix? Your video files on disk will NOT be deleted.`)) {
        return;
      }
      try {
        await api(`/api/shows/${btn.dataset.deleteShow}`, { method: "DELETE" });
        showToast("Removed show from library");
        await loadLibrary();
      } catch (err) {
        showToast(`Remove failed: ${err.message}`);
      }
    });
  });
}

/* ==================== WATCH VIEW & PLAYER ==================== */

async function openShow(showId, opts = {}) {
  try {
    // Auto-sync on open so newly added DVD rips appear immediately without losing progress
    const data = await api(`/api/shows/${showId}?autosync=1`);
    state.activeShow = data.show;
    state.linkedList = data.linked_list;
    setView("watch");

    if (data.diff && (data.diff.added > 0 || data.diff.removed > 0)) {
      showToast(`Folder updated: +${data.diff.added} / -${data.diff.removed} files`);
    }

    if (opts.advanceNextImmediately) {
      const nextData = await api(`/api/shows/${showId}/next`, { method: "POST", body: "{}" });
      state.linkedList = nextData.linked_list;
    }

    const current = getEpisodeById(state.linkedList.current_id) || state.linkedList.episodes[0];
    if (!current) {
      showToast("No playable video files found in this folder.");
      renderEpisodeList();
      return;
    }

    loadEpisode(current, { autoPlay: Boolean(opts.autoPlay) });
  } catch (err) {
    showToast(`Could not open show: ${err.message}`);
  }
}

function loadEpisode(episode, { autoPlay = true, forceStartZero = false } = {}) {
  state.currentEpisode = episode;
  state.upNextDismissedFor = null;
  state.autoPlayOnLoad = autoPlay;

  els.playerErrorBanner.classList.add("hidden");
  els.upNextCard.classList.add("hidden");

  // Determine resume timeplace: if user was near the end (>=95%), start cleanly at 0:00
  let startAt = 0;
  if (!forceStartZero && episode.position_sec > 1) {
    const nearEnd = episode.duration_sec > 0 && episode.position_sec >= episode.duration_sec * 0.95;
    startAt = nearEnd ? 0 : episode.position_sec;
  }
  state.pendingSeekSec = startAt;

  // Update header & metadata
  const idx = (episode.index ?? 0) + 1;
  const total = state.linkedList ? state.linkedList.length : 1;
  els.watchShowName.textContent = state.activeShow.name;
  els.watchChainPos.textContent = `Linked List Node ${idx} of ${total}`;
  els.watchEpCode.textContent = episode.display_code;
  els.watchEpTitle.textContent = episode.title;
  els.metaRelativePath.textContent = episode.relative_path;
  els.metaSortReason.textContent = `${episode.sort_reason} (${Math.round(episode.confidence * 100)}% confidence)`;

  const prevNode = getEpisodeById(episode.prev_id);
  const nextNode = getEpisodeById(episode.next_id);

  els.metaChainLinks.textContent = `${prevNode ? prevNode.display_code : "HEAD"} ← ${episode.display_code} → ${nextNode ? nextNode.display_code : "TAIL"}`;

  els.prevEpBtn.disabled = !prevNode;
  els.prevEpBtn.textContent = prevNode ? `← ${prevNode.display_code}` : "← Prev";

  els.nextEpBtn.disabled = !nextNode;
  els.nextEpBtn.textContent = nextNode ? `Next: ${nextNode.display_code} →` : "End of Show";

  // Show the quick Next Episode button right at the start of playback when there is a next node
  if (nextNode) {
    els.quickNextTitle.textContent = `${nextNode.display_code} · ${nextNode.title}`;
    els.quickNextOverlay.classList.remove("hidden");
    els.upNextTitle.textContent = `${nextNode.display_code} · ${nextNode.title}`;
  } else {
    els.quickNextOverlay.classList.add("hidden");
  }

  els.saveStatusPill.textContent =
    startAt > 1 ? `Resuming ${formatTime(startAt)}` : "Starting at 0:00";

  // Set video source
  const streamUrl = `/api/episodes/${episode.id}/stream`;
  els.videoPlayer.pause();
  els.videoPlayer.src = streamUrl;
  els.videoPlayer.load();

  renderEpisodeList();
}

function renderEpisodeList() {
  if (!state.linkedList || !state.activeShow) return;
  const eps = state.linkedList.episodes;
  const activeId = state.currentEpisode ? state.currentEpisode.id : state.linkedList.current_id;

  els.chainSummary.textContent = `${eps.length} node${eps.length === 1 ? "" : "s"} · ${
    state.activeShow.manual_order ? "Manual linked order" : "Auto-ordered by intelligence"
  }`;

  if (state.activeShow.manual_order) {
    els.resetSmartSortBtn.classList.remove("hidden");
  } else {
    els.resetSmartSortBtn.classList.add("hidden");
  }

  els.episodeList.innerHTML = eps
    .map((ep, idx) => {
      const isActive = ep.id === activeId;
      const statusHtml = ep.watched
        ? `<span class="watched-tag">✓ Watched</span>`
        : ep.position_sec > 2
        ? `<span>${formatTime(ep.position_sec)}${ep.duration_sec > 0 ? ` / ${formatTime(ep.duration_sec)}` : ""}</span>`
        : `<span>Unwatched</span>`;

      return `
        <div class="ep-node-card ${isActive ? "active" : ""}" data-select-ep="${ep.id}">
          <div class="ep-node-top">
            <div class="ep-node-left">
              <span class="ep-idx">#${idx + 1}</span>
              <span class="ep-mini-code ${ep.is_extra ? "extra" : ""}">${escapeHtml(ep.display_code)}</span>
            </div>
            <div class="ep-node-controls">
              <button class="reorder-btn" data-move-ep="${ep.id}" data-target-idx="${idx - 1}" type="button" title="Move earlier in linked list" ${idx === 0 ? "disabled" : ""}>↑</button>
              <button class="reorder-btn" data-move-ep="${ep.id}" data-target-idx="${idx + 1}" type="button" title="Move later in linked list" ${idx === eps.length - 1 ? "disabled" : ""}>↓</button>
            </div>
          </div>
          <div class="ep-node-title" title="${escapeHtml(ep.filename)}">${escapeHtml(ep.title)}</div>
          <div class="ep-node-sub">
            <span class="ep-reason" title="${escapeHtml(ep.sort_reason)}">${escapeHtml(ep.sort_reason)}</span>
            ${statusHtml}
          </div>
          <div class="progress-track">
            <div class="progress-fill" style="width: ${ep.progress_percent}%"></div>
          </div>
        </div>
        ${idx < eps.length - 1 ? `<div class="chain-arrow">↓</div>` : ""}
      `;
    })
    .join("");

  els.episodeList.querySelectorAll("[data-select-ep]").forEach((card) => {
    card.addEventListener("click", async (ev) => {
      if (ev.target.closest("[data-move-ep]")) return;
      const epId = card.dataset.selectEp;
      if (state.currentEpisode && state.currentEpisode.id === epId) return;
      await saveCurrentProgress({ immediate: true });
      try {
        const res = await api(`/api/shows/${state.activeShow.id}/select`, {
          method: "POST",
          body: JSON.stringify({ episode_id: epId }),
        });
        state.linkedList = res.linked_list;
        loadEpisode(res.episode, { autoPlay: true });
      } catch (err) {
        showToast(`Failed to switch episode: ${err.message}`);
      }
    });
  });

  els.episodeList.querySelectorAll("[data-move-ep]").forEach((btn) => {
    btn.addEventListener("click", async (ev) => {
      ev.stopPropagation();
      const epId = btn.dataset.moveEp;
      const targetIdx = Number(btn.dataset.targetIdx);
      try {
        const res = await api(`/api/shows/${state.activeShow.id}/reorder`, {
          method: "POST",
          body: JSON.stringify({ episode_id: epId, target_index: targetIdx }),
        });
        state.activeShow.manual_order = true;
        state.linkedList = res.linked_list;
        const refreshed = getEpisodeById(state.currentEpisode?.id);
        if (refreshed) {
          state.currentEpisode = refreshed;
          refreshCurrentLinksUI();
        }
        renderEpisodeList();
      } catch (err) {
        showToast(`Reorder failed: ${err.message}`);
      }
    });
  });

  const activeEl = els.episodeList.querySelector(".ep-node-card.active");
  if (activeEl) {
    activeEl.scrollIntoView({ block: "nearest", behavior: "smooth" });
  }
}

function refreshCurrentLinksUI() {
  const ep = state.currentEpisode;
  if (!ep || !state.linkedList) return;
  const idx = (ep.index ?? 0) + 1;
  els.watchChainPos.textContent = `Linked List Node ${idx} of ${state.linkedList.length}`;
  const prevNode = getEpisodeById(ep.prev_id);
  const nextNode = getEpisodeById(ep.next_id);
  els.metaChainLinks.textContent = `${prevNode ? prevNode.display_code : "HEAD"} ← ${ep.display_code} → ${nextNode ? nextNode.display_code : "TAIL"}`;
  els.prevEpBtn.disabled = !prevNode;
  els.prevEpBtn.textContent = prevNode ? `← ${prevNode.display_code}` : "← Prev";
  els.nextEpBtn.disabled = !nextNode;
  els.nextEpBtn.textContent = nextNode ? `Next: ${nextNode.display_code} →` : "End of Show";
  if (nextNode) {
    els.quickNextTitle.textContent = `${nextNode.display_code} · ${nextNode.title}`;
    els.quickNextOverlay.classList.remove("hidden");
    els.upNextTitle.textContent = `${nextNode.display_code} · ${nextNode.title}`;
  } else {
    els.quickNextOverlay.classList.add("hidden");
  }
}

async function saveCurrentProgress({ immediate = false, watched = null, useBeacon = false } = {}) {
  const ep = state.currentEpisode;
  if (!ep) return;
  const now = Date.now();
  if (!immediate && now - state.lastSavedAt < 4500) return;

  const pos = Number.isFinite(els.videoPlayer.currentTime) ? els.videoPlayer.currentTime : ep.position_sec;
  const dur = Number.isFinite(els.videoPlayer.duration) && els.videoPlayer.duration > 0
    ? els.videoPlayer.duration
    : ep.duration_sec;

  const payload = {
    position_sec: Math.max(0, pos),
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
    if (state.linkedList) {
      const idx = state.linkedList.episodes.findIndex((item) => item.id === updated.id);
      if (idx >= 0) {
        state.linkedList.episodes[idx] = { ...state.linkedList.episodes[idx], ...updated };
      }
    }
    if (state.currentEpisode && state.currentEpisode.id === updated.id) {
      state.currentEpisode = { ...state.currentEpisode, ...updated };
    }
    els.saveStatusPill.textContent = `Saved ${formatTime(updated.position_sec)}`;
  } catch {
    // Non-fatal during periodic saves
  }
}

async function goNextEpisode() {
  if (!state.activeShow) return;
  await saveCurrentProgress({ immediate: true });
  try {
    const res = await api(`/api/shows/${state.activeShow.id}/next`, {
      method: "POST",
      body: "{}",
    });
    state.linkedList = res.linked_list;
    if (!res.episode) {
      showToast("You've reached the last episode in this show's linked list.");
      return;
    }
    loadEpisode(res.episode, { autoPlay: true });
  } catch (err) {
    showToast(`Could not go to next episode: ${err.message}`);
  }
}

async function goPrevEpisode() {
  if (!state.activeShow) return;
  await saveCurrentProgress({ immediate: true });
  try {
    const res = await api(`/api/shows/${state.activeShow.id}/prev`, {
      method: "POST",
      body: "{}",
    });
    state.linkedList = res.linked_list;
    if (!res.episode) {
      showToast("Already at the first episode.");
      return;
    }
    loadEpisode(res.episode, { autoPlay: true });
  } catch (err) {
    showToast(`Could not go to previous episode: ${err.message}`);
  }
}

async function backToLibrary() {
  await saveCurrentProgress({ immediate: true });
  els.videoPlayer.pause();
  setView("library");
  await loadLibrary();
}

/* ==================== VIDEO EVENTS ==================== */

els.videoPlayer.addEventListener("loadedmetadata", () => {
  if (state.pendingSeekSec !== null && state.pendingSeekSec > 0) {
    const maxSeek = Math.max(0, (els.videoPlayer.duration || state.pendingSeekSec) - 0.5);
    els.videoPlayer.currentTime = Math.min(state.pendingSeekSec, maxSeek);
  }
  state.pendingSeekSec = null;

  if (state.autoPlayOnLoad) {
    els.videoPlayer.play().catch(() => {
      // Browser autoplay policy may require a click; controls remain visible
    });
  }
});

els.videoPlayer.addEventListener("timeupdate", () => {
  saveCurrentProgress({ immediate: false });

  const dur = els.videoPlayer.duration;
  const cur = els.videoPlayer.currentTime;
  const ep = state.currentEpisode;
  if (ep && ep.next_id && Number.isFinite(dur) && dur > 20) {
    const remaining = dur - cur;
    if (remaining <= 15 && state.upNextDismissedFor !== ep.id) {
      els.upNextCard.classList.remove("hidden");
    }
  }
});

els.videoPlayer.addEventListener("pause", () => {
  saveCurrentProgress({ immediate: true });
});

els.videoPlayer.addEventListener("seeked", () => {
  saveCurrentProgress({ immediate: true });
});

els.videoPlayer.addEventListener("ended", async () => {
  await saveCurrentProgress({ immediate: true, watched: true });
  renderEpisodeList();
  if (state.currentEpisode && state.currentEpisode.next_id) {
    els.upNextCard.classList.remove("hidden");
  }
});

els.videoPlayer.addEventListener("error", () => {
  const ep = state.currentEpisode;
  if (!ep) return;
  els.playerErrorBanner.classList.remove("hidden");
  els.playerErrorPath.textContent = ep.file_path;
  els.playerErrorMsg.textContent = state.ffmpegAvailable
    ? "Your browser could not natively decode this DVD rip's video/audio codec (common with raw .vob, .avi, or AC3 audio). Remuxing to H.264/AAC .mp4 will make it play smoothly in-browser."
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

/* ==================== KEYBOARD SHORTCUTS ==================== */

document.addEventListener("keydown", (ev) => {
  if (state.view !== "watch") return;
  const tag = (ev.target && ev.target.tagName) || "";
  if (["INPUT", "TEXTAREA", "SELECT", "BUTTON"].includes(tag)) return;

  if (ev.code === "Space" || ev.key === "k" || ev.key === "K") {
    ev.preventDefault();
    if (els.videoPlayer.paused) {
      els.videoPlayer.play().catch(() => {});
    } else {
      els.videoPlayer.pause();
    }
  } else if (ev.key === "ArrowRight") {
    ev.preventDefault();
    els.videoPlayer.currentTime = Math.min(
      els.videoPlayer.duration || Infinity,
      els.videoPlayer.currentTime + 10
    );
  } else if (ev.key === "ArrowLeft") {
    ev.preventDefault();
    els.videoPlayer.currentTime = Math.max(0, els.videoPlayer.currentTime - 10);
  } else if (ev.key === "n" || ev.key === "N") {
    ev.preventDefault();
    goNextEpisode();
  } else if (ev.key === "p" || ev.key === "P") {
    ev.preventDefault();
    goPrevEpisode();
  } else if (ev.key === "r" || ev.key === "R") {
    ev.preventDefault();
    els.videoPlayer.currentTime = 0;
    saveCurrentProgress({ immediate: true });
  } else if (ev.key === "f" || ev.key === "F") {
    ev.preventDefault();
    if (document.fullscreenElement) {
      document.exitFullscreen().catch(() => {});
    } else {
      els.videoPlayer.requestFullscreen?.().catch(() => {});
    }
  }
});

/* ==================== ADD FOLDER MODAL & BROWSER ==================== */

async function openAddModal() {
  els.addModalError.classList.add("hidden");
  els.showNameInput.value = "";
  els.addModalBackdrop.classList.remove("hidden");
  await browseDirectory(els.folderPathInput.value.trim());
}

function closeAddModal() {
  els.addModalBackdrop.classList.add("hidden");
}

async function browseDirectory(path = "") {
  els.addModalError.classList.add("hidden");
  try {
    const query = path ? `?path=${encodeURIComponent(path)}` : "";
    const data = await api(`/api/fs/browse${query}`);
    state.browseParent = data.parent;
    els.folderPathInput.value = data.path;
    els.browserCurrentPath.textContent = data.path;
    els.browseUpBtn.disabled = !data.parent;
    els.browserVideoBadge.textContent =
      data.direct_video_count > 0
        ? `${data.direct_video_count} video file${data.direct_video_count === 1 ? "" : "s"} here`
        : "";

    if (!data.directories.length) {
      els.browserList.innerHTML = `<div class="muted small" style="padding: 0.65rem;">No subfolders here. Click "Scan &amp; Add Folder" to add this directory.</div>`;
      return;
    }

    els.browserList.innerHTML = data.directories
      .map(
        (dir) => `
          <button class="browser-item" type="button" data-dir-path="${escapeHtml(dir.path)}">
            <span>📁</span>
            <span>${escapeHtml(dir.name)}</span>
          </button>
        `
      )
      .join("");

    els.browserList.querySelectorAll("[data-dir-path]").forEach((btn) => {
      btn.addEventListener("click", () => browseDirectory(btn.dataset.dirPath));
    });
  } catch (err) {
    els.addModalError.textContent = err.message;
    els.addModalError.classList.remove("hidden");
  }
}

async function confirmAddShow() {
  const folderPath = els.folderPathInput.value.trim();
  const customName = els.showNameInput.value.trim();
  if (!folderPath) {
    els.addModalError.textContent = "Please choose or enter a folder path.";
    els.addModalError.classList.remove("hidden");
    return;
  }

  els.confirmAddShowBtn.disabled = true;
  els.confirmAddShowBtn.textContent = "Scanning...";
  els.addModalError.classList.add("hidden");

  try {
    const res = await api("/api/shows", {
      method: "POST",
      body: JSON.stringify({ folder_path: folderPath, name: customName || null }),
    });
    closeAddModal();
    showToast(`Added "${res.show.name}" (${res.linked_list.length} episodes ordered)`);
    await loadLibrary();
    if (res.linked_list.length > 0) {
      await openShow(res.show.id, { autoPlay: false });
    }
  } catch (err) {
    els.addModalError.textContent = err.message;
    els.addModalError.classList.remove("hidden");
  } finally {
    els.confirmAddShowBtn.disabled = false;
    els.confirmAddShowBtn.textContent = "Scan & Add Folder";
  }
}

/* ==================== BUTTON BINDINGS ==================== */

els.brandBtn.addEventListener("click", backToLibrary);
els.navLibraryBtn.addEventListener("click", backToLibrary);
els.openAddModalBtn.addEventListener("click", openAddModal);
els.emptyAddBtn.addEventListener("click", openAddModal);

els.closeAddModalBtn.addEventListener("click", closeAddModal);
els.cancelAddModalBtn.addEventListener("click", closeAddModal);
els.addModalBackdrop.addEventListener("click", (ev) => {
  if (ev.target === els.addModalBackdrop) closeAddModal();
});
els.browseGoBtn.addEventListener("click", () => browseDirectory(els.folderPathInput.value.trim()));
els.folderPathInput.addEventListener("keydown", (ev) => {
  if (ev.key === "Enter") browseDirectory(els.folderPathInput.value.trim());
});
els.browseUpBtn.addEventListener("click", () => {
  if (state.browseParent) browseDirectory(state.browseParent);
});
els.confirmAddShowBtn.addEventListener("click", confirmAddShow);

els.nextEpBtn.addEventListener("click", goNextEpisode);
els.quickNextBtn.addEventListener("click", goNextEpisode);
els.upNextPlayBtn.addEventListener("click", goNextEpisode);
els.prevEpBtn.addEventListener("click", goPrevEpisode);

els.upNextDismissBtn.addEventListener("click", () => {
  if (state.currentEpisode) state.upNextDismissedFor = state.currentEpisode.id;
  els.upNextCard.classList.add("hidden");
});

els.restartEpBtn.addEventListener("click", () => {
  els.videoPlayer.currentTime = 0;
  saveCurrentProgress({ immediate: true });
  els.videoPlayer.play().catch(() => {});
});

els.rescanShowBtn.addEventListener("click", async () => {
  if (!state.activeShow) return;
  try {
    const res = await api(`/api/shows/${state.activeShow.id}/rescan`, {
      method: "POST",
      body: JSON.stringify({ reset_order: false }),
    });
    state.activeShow = res.show;
    state.linkedList = res.linked_list;
    const current = getEpisodeById(state.linkedList.current_id);
    if (current) {
      state.currentEpisode = current;
      refreshCurrentLinksUI();
    }
    renderEpisodeList();
    showToast(`Rescanned: +${res.diff.added} added, -${res.diff.removed} removed`);
  } catch (err) {
    showToast(`Rescan failed: ${err.message}`);
  }
});

els.resetSmartSortBtn.addEventListener("click", async () => {
  if (!state.activeShow) return;
  try {
    const res = await api(`/api/shows/${state.activeShow.id}/rescan`, {
      method: "POST",
      body: JSON.stringify({ reset_order: true }),
    });
    state.activeShow = res.show;
    state.linkedList = res.linked_list;
    const current = getEpisodeById(state.linkedList.current_id);
    if (current) {
      state.currentEpisode = current;
      refreshCurrentLinksUI();
    }
    renderEpisodeList();
    showToast("Restored intelligent episode order");
  } catch (err) {
    showToast(`Smart sort failed: ${err.message}`);
  }
});

/* ==================== INIT ==================== */

async function init() {
  try {
    const health = await api("/api/health");
    state.ffmpegAvailable = Boolean(health.ffmpeg_available);
  } catch {
    // ignore
  }
  await loadLibrary();
}

init();
