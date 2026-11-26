import {
  api,
  byId,
  escapeHtml,
  folderBasename,
  formatTime,
  showMonogram,
  showPosterStyle,
  showToast,
} from "./utils.js";
import { state } from "./state.js";

let callbacks = {
  onOpenShow: async () => {},
};

export async function loadLibrary() {
  try {
    const data = await api("/api/shows");
    state.shows = data.shows || [];
    renderLibrary();
  } catch (err) {
    showToast(`Failed to load library: ${err.message}`);
  }
}

function getFilteredSortedShows() {
  const q = state.libraryQuery.trim().toLowerCase();
  let list = state.shows.filter((s) => !q || s.name.toLowerCase().includes(q));
  if (state.librarySort === "alpha") {
    list = [...list].sort((a, b) => a.name.localeCompare(b.name, undefined, { sensitivity: "base" }));
  } else if (state.librarySort === "progress") {
    list = [...list].sort((a, b) => {
      const pa = a.episode_count > 0 ? a.watched_count / a.episode_count : 0;
      const pb = b.episode_count > 0 ? b.watched_count / b.episode_count : 0;
      return pb - pa || a.name.localeCompare(b.name);
    });
  }
  return list;
}

function formatShowCountPill(show) {
  const parts = [];
  if (show.season_count > 0) {
    parts.push(`${show.season_count} season${show.season_count === 1 ? "" : "s"}`);
  } else if (show.disc_count > 0) {
    parts.push(`${show.disc_count} disc${show.disc_count === 1 ? "" : "s"}`);
  }
  const mainCount = show.main_episode_count ?? show.episode_count;
  parts.push(`${mainCount} episode${mainCount === 1 ? "" : "s"}`);
  if (show.extra_count > 0) {
    parts.push(`+${show.extra_count} extra${show.extra_count === 1 ? "" : "s"}`);
  }
  return parts.join(" · ");
}

function renderContinueHero(allShows) {
  const continueHero = byId("continueHero");
  const heroShow = allShows.find((s) => s.current_episode) || allShows[0];
  if (!heroShow || !heroShow.current_episode) {
    continueHero.classList.add("hidden");
    return;
  }

  const ep = heroShow.current_episode;
  const nxt = heroShow.next_episode;
  const timeLabel =
    ep.position_sec > 2
      ? `Paused at ${formatTime(ep.position_sec)}${ep.duration_sec > 0 ? ` / ${formatTime(ep.duration_sec)}` : ""}`
      : "Ready to start";
  const seasonInfo =
    heroShow.season_count > 0
      ? `${heroShow.season_count} season${heroShow.season_count === 1 ? "" : "s"} · `
      : "";

  continueHero.classList.remove("hidden");
  continueHero.innerHTML = `
    <div class="hero-content">
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
        <div class="hero-substats">${seasonInfo}${heroShow.watched_count} of ${heroShow.episode_count} watched</div>
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
}

export function renderLibrary() {
  const allShows = state.shows;
  const showsGrid = byId("showsGrid");
  const libraryNoResults = byId("libraryNoResults");

  if (allShows.length === 0) {
    byId("continueHero").classList.add("hidden");
    byId("libraryHeader").classList.add("hidden");
    libraryNoResults.classList.add("hidden");
    showsGrid.innerHTML = "";
    byId("emptyLibrary").classList.remove("hidden");
    return;
  }

  byId("emptyLibrary").classList.add("hidden");
  byId("libraryHeader").classList.remove("hidden");
  byId("libraryToolbar").classList.toggle("hidden", allShows.length < 2);

  renderContinueHero(allShows);

  const shows = getFilteredSortedShows();
  if (shows.length === 0) {
    showsGrid.innerHTML = "";
    libraryNoResults.classList.remove("hidden");
    return;
  }
  libraryNoResults.classList.add("hidden");

  showsGrid.innerHTML = shows
    .map((show) => {
      const ep = show.current_episode;
      const pctShow =
        show.episode_count > 0 ? Math.round((show.watched_count / show.episode_count) * 100) : 0;
      const epInfoHtml = ep
        ? `
          <div class="show-current-box" data-open-show="${show.id}" title="Click to resume">
            <div class="show-current-row">
              <span class="show-current-label"><strong>${escapeHtml(ep.display_code)}</strong> · ${escapeHtml(ep.title)}</span>
              <span class="muted">${ep.position_sec > 2 ? formatTime(ep.position_sec) : "Start"}</span>
            </div>
            <div class="progress-track">
              <div class="progress-fill ${ep.watched ? "complete" : ""}" style="width: ${ep.progress_percent}%"></div>
            </div>
          </div>
        `
        : `<div class="show-current-box muted">No video files found in this folder yet.</div>`;

      return `
        <article class="show-card">
          <div class="show-card-top">
            <div class="show-poster" style="background: ${showPosterStyle(show.name)}" data-open-show="${show.id}" aria-hidden="true">
              ${escapeHtml(showMonogram(show.name))}
            </div>
            <div class="show-card-meta">
              <div class="show-card-title-row">
                <h3 class="show-card-title" data-open-show="${show.id}" title="${escapeHtml(show.name)}">${escapeHtml(show.name)}</h3>
                <button class="icon-btn" data-rename-show="${show.id}" type="button" title="Rename show" aria-label="Rename ${escapeHtml(show.name)}">✎</button>
              </div>
              <div class="show-card-path" title="${escapeHtml(show.folder_path)}">${escapeHtml(show.folder_path)}</div>
            </div>
          </div>
          <div class="show-stats">
            <span class="pill">${escapeHtml(formatShowCountPill(show))}</span>
            <span class="pill">${show.watched_count}/${show.episode_count} watched (${pctShow}%)</span>
            ${show.manual_order ? `<span class="pill pill-accent">Custom Order</span>` : ""}
            ${!show.folder_exists ? `<span class="pill pill-warn">Folder Missing</span>` : ""}
          </div>
          ${epInfoHtml}
          <div class="show-card-actions">
            <button class="btn btn-primary btn-sm" data-open-show="${show.id}" type="button" ${show.episode_count === 0 ? "disabled" : ""}>
              ▶ ${ep && ep.position_sec > 2 ? "Resume" : "Watch"}
            </button>
            <button class="btn btn-secondary btn-sm" data-rescan-show="${show.id}" type="button" title="Scan folder for added or removed episodes">
              ↻ Rescan
            </button>
            <button class="btn btn-danger btn-sm" data-delete-show="${show.id}" type="button" title="Remove from Notflix (keeps files on disk)" aria-label="Remove ${escapeHtml(show.name)}">
              ✕
            </button>
          </div>
        </article>
      `;
    })
    .join("");
}

/* ==================== MODALS (ADD, RENAME, DELETE) ==================== */

export function isAnyModalOpen() {
  return (
    !byId("addModalBackdrop").classList.contains("hidden") ||
    !byId("renameModalBackdrop").classList.contains("hidden") ||
    !byId("confirmModalBackdrop").classList.contains("hidden")
  );
}

export function closeOpenModal() {
  if (!byId("addModalBackdrop").classList.contains("hidden")) {
    closeAddModal();
    return true;
  }
  if (!byId("renameModalBackdrop").classList.contains("hidden")) {
    closeRenameModal();
    return true;
  }
  if (!byId("confirmModalBackdrop").classList.contains("hidden")) {
    closeDeleteModal();
    return true;
  }
  return false;
}

function updateAddModalFolderLabels(pathStr) {
  const base = folderBasename(pathStr);
  byId("showNameInput").placeholder = base ? `Default: ${base}` : "Defaults to selected folder name";
  byId("confirmAddShowBtn").textContent = base ? `Add "${base}"` : "Add Folder";
}

function renderBrowserDirs() {
  const q = byId("browserFilterInput").value.trim().toLowerCase();
  const dirs = state.browseDirs.filter((d) => !q || d.name.toLowerCase().includes(q));
  const browserList = byId("browserList");

  if (!state.browseDirs.length) {
    browserList.innerHTML = `<div class="muted small" style="padding: 0.7rem;">No subfolders here. Click the button below to add this folder.</div>`;
    return;
  }
  if (!dirs.length) {
    browserList.innerHTML = `<div class="muted small" style="padding: 0.7rem;">No subfolders match "${escapeHtml(q)}".</div>`;
    return;
  }
  browserList.innerHTML = dirs
    .map(
      (dir) => `
        <button class="browser-item" type="button" data-dir-path="${escapeHtml(dir.path)}">
          <span>📁</span>
          <span>${escapeHtml(dir.name)}</span>
        </button>
      `
    )
    .join("");
}

async function browseDirectory(path = "") {
  const addModalError = byId("addModalError");
  const browserList = byId("browserList");
  addModalError.classList.add("hidden");
  browserList.innerHTML = `<div class="muted small" style="padding: 0.7rem;">Loading folders…</div>`;

  try {
    const query = path ? `?path=${encodeURIComponent(path)}` : "";
    const data = await api(`/api/fs/browse${query}`);
    state.browseParent = data.parent;
    state.browseDirs = data.directories || [];
    byId("folderPathInput").value = data.path;
    const browserCurrentPath = byId("browserCurrentPath");
    browserCurrentPath.textContent = data.path;
    browserCurrentPath.title = data.path;
    byId("browseUpBtn").disabled = !data.parent;
    byId("browserFilterInput").value = "";
    updateAddModalFolderLabels(data.path);

    const badge = byId("browserVideoBadge");
    if (data.direct_video_count > 0) {
      badge.textContent = `${data.direct_video_count} video${data.direct_video_count === 1 ? "" : "s"} here`;
      badge.classList.remove("is-hint");
    } else if (state.browseDirs.length > 0) {
      badge.textContent = "Subfolders scanned too";
      badge.classList.add("is-hint");
    } else {
      badge.textContent = "";
    }

    byId("browserFilterRow").classList.toggle("hidden", state.browseDirs.length <= 8);
    renderBrowserDirs();
  } catch (err) {
    browserList.innerHTML = "";
    addModalError.textContent = err.message;
    addModalError.classList.remove("hidden");
  }
}

export async function openAddModal() {
  byId("addModalError").classList.add("hidden");
  byId("showNameInput").value = "";
  byId("browserFilterInput").value = "";
  byId("addModalBackdrop").classList.remove("hidden");
  const folderPathInput = byId("folderPathInput");
  const startPath = state.lastAddedParent || folderPathInput.value.trim();
  await browseDirectory(startPath);
  folderPathInput.focus();
}

function closeAddModal() {
  byId("addModalBackdrop").classList.add("hidden");
}

async function confirmAddShow() {
  const folderPathInput = byId("folderPathInput");
  const folderPath = folderPathInput.value.trim();
  const customName = byId("showNameInput").value.trim();
  const addModalError = byId("addModalError");
  const confirmBtn = byId("confirmAddShowBtn");

  if (!folderPath) {
    addModalError.textContent = "Please choose or enter a folder path.";
    addModalError.classList.remove("hidden");
    return;
  }

  confirmBtn.disabled = true;
  confirmBtn.textContent = "Scanning…";
  addModalError.classList.add("hidden");

  try {
    const res = await api("/api/shows", {
      method: "POST",
      body: JSON.stringify({ folder_path: folderPath, name: customName || null }),
    });
    state.lastAddedParent = state.browseParent || "";
    closeAddModal();
    showToast(
      `Added "${res.show.name}" (${res.linked_list.length} episode${res.linked_list.length === 1 ? "" : "s"})`,
      "success"
    );
    await loadLibrary();
    if (res.linked_list.length > 0) {
      await callbacks.onOpenShow(res.show.id, { autoPlay: false });
    }
  } catch (err) {
    addModalError.textContent = err.message;
    addModalError.classList.remove("hidden");
  } finally {
    confirmBtn.disabled = false;
    updateAddModalFolderLabels(folderPathInput.value.trim());
  }
}

function openRenameModal(showId) {
  const show = state.shows.find((s) => s.id === showId);
  if (!show) return;
  state.pendingRenameShowId = showId;
  const input = byId("renameShowInput");
  input.value = show.name;
  byId("renameModalError").classList.add("hidden");
  byId("renameModalBackdrop").classList.remove("hidden");
  input.focus();
  input.select();
}

function closeRenameModal() {
  state.pendingRenameShowId = null;
  byId("renameModalBackdrop").classList.add("hidden");
}

async function confirmRenameShow() {
  const showId = state.pendingRenameShowId;
  const newName = byId("renameShowInput").value.trim();
  const renameModalError = byId("renameModalError");
  if (!showId || !newName) {
    renameModalError.textContent = "Please enter a show name.";
    renameModalError.classList.remove("hidden");
    return;
  }
  const btn = byId("confirmRenameShowBtn");
  btn.disabled = true;
  try {
    await api(`/api/shows/${showId}`, {
      method: "PATCH",
      body: JSON.stringify({ name: newName }),
    });
    closeRenameModal();
    showToast(`Renamed show to "${newName}"`, "success");
    await loadLibrary();
  } catch (err) {
    renameModalError.textContent = err.message;
    renameModalError.classList.remove("hidden");
  } finally {
    btn.disabled = false;
  }
}

function openDeleteModal(showId) {
  const show = state.shows.find((s) => s.id === showId);
  if (!show) return;
  state.pendingDeleteShowId = showId;
  byId("confirmModalMessage").textContent = `Remove "${show.name}" from your Notflix library?`;
  byId("confirmModalBackdrop").classList.remove("hidden");
  byId("cancelConfirmModalBtn").focus();
}

function closeDeleteModal() {
  state.pendingDeleteShowId = null;
  byId("confirmModalBackdrop").classList.add("hidden");
}

async function confirmDeleteShow() {
  const showId = state.pendingDeleteShowId;
  if (!showId) return;
  const btn = byId("confirmDeleteShowBtn");
  btn.disabled = true;
  try {
    await api(`/api/shows/${showId}`, { method: "DELETE" });
    closeDeleteModal();
    showToast("Removed show from library");
    await loadLibrary();
  } catch (err) {
    showToast(`Remove failed: ${err.message}`);
  } finally {
    btn.disabled = false;
  }
}

export function initLibrary(hooks = {}) {
  callbacks = { ...callbacks, ...hooks };

  // Delegated Continue Watching hero actions
  byId("continueHero").addEventListener("click", async (ev) => {
    const watchBtn = ev.target.closest("[data-hero-watch]");
    if (watchBtn) {
      callbacks.onOpenShow(watchBtn.dataset.heroWatch, { autoPlay: true });
      return;
    }
    const nextBtn = ev.target.closest("[data-hero-next]");
    if (nextBtn) {
      await callbacks.onOpenShow(nextBtn.dataset.heroNext, {
        advanceNextImmediately: true,
        autoPlay: true,
      });
    }
  });

  // Delegated show card actions
  byId("showsGrid").addEventListener("click", async (ev) => {
    const renameBtn = ev.target.closest("[data-rename-show]");
    if (renameBtn) {
      ev.stopPropagation();
      openRenameModal(renameBtn.dataset.renameShow);
      return;
    }

    const rescanBtn = ev.target.closest("[data-rescan-show]");
    if (rescanBtn) {
      if (rescanBtn.disabled) return;
      rescanBtn.disabled = true;
      const orig = rescanBtn.textContent;
      rescanBtn.textContent = "Scanning…";
      try {
        const res = await api(`/api/shows/${rescanBtn.dataset.rescanShow}/rescan`, {
          method: "POST",
          body: JSON.stringify({ reset_order: false }),
        });
        showToast(
          `Rescanned: +${res.diff.added} added, -${res.diff.removed} removed (${res.diff.total} total)`,
          "success"
        );
        await loadLibrary();
      } catch (err) {
        showToast(`Rescan failed: ${err.message}`);
        rescanBtn.disabled = false;
        rescanBtn.textContent = orig;
      }
      return;
    }

    const deleteBtn = ev.target.closest("[data-delete-show]");
    if (deleteBtn) {
      openDeleteModal(deleteBtn.dataset.deleteShow);
      return;
    }

    const openEl = ev.target.closest("[data-open-show]");
    if (openEl) {
      const show = state.shows.find((s) => s.id === openEl.dataset.openShow);
      if (show && show.episode_count === 0) return;
      callbacks.onOpenShow(openEl.dataset.openShow, { autoPlay: true });
    }
  });

  // Delegated folder browser directory clicks
  byId("browserList").addEventListener("click", (ev) => {
    const btn = ev.target.closest("[data-dir-path]");
    if (!btn) return;
    browseDirectory(btn.dataset.dirPath);
  });

  // Library search & sort
  const librarySearchInput = byId("librarySearchInput");
  librarySearchInput.addEventListener("input", () => {
    state.libraryQuery = librarySearchInput.value;
    renderLibrary();
  });

  const librarySortSelect = byId("librarySortSelect");
  librarySortSelect.addEventListener("change", () => {
    state.librarySort = librarySortSelect.value;
    renderLibrary();
  });

  byId("clearLibrarySearchBtn").addEventListener("click", () => {
    state.libraryQuery = "";
    librarySearchInput.value = "";
    renderLibrary();
  });

  // Add modal controls
  const addBackdrop = byId("addModalBackdrop");
  const folderPathInput = byId("folderPathInput");
  byId("closeAddModalBtn").addEventListener("click", closeAddModal);
  byId("cancelAddModalBtn").addEventListener("click", closeAddModal);
  addBackdrop.addEventListener("click", (ev) => {
    if (ev.target === addBackdrop) closeAddModal();
  });
  byId("browseGoBtn").addEventListener("click", () => browseDirectory(folderPathInput.value.trim()));
  byId("browseHomeBtn").addEventListener("click", () => browseDirectory(""));
  folderPathInput.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") browseDirectory(folderPathInput.value.trim());
  });
  byId("showNameInput").addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") confirmAddShow();
  });
  byId("browseUpBtn").addEventListener("click", () => {
    if (state.browseParent) browseDirectory(state.browseParent);
  });
  byId("browserFilterInput").addEventListener("input", renderBrowserDirs);
  byId("confirmAddShowBtn").addEventListener("click", confirmAddShow);

  // Rename modal controls
  const renameBackdrop = byId("renameModalBackdrop");
  byId("closeRenameModalBtn").addEventListener("click", closeRenameModal);
  byId("cancelRenameModalBtn").addEventListener("click", closeRenameModal);
  renameBackdrop.addEventListener("click", (ev) => {
    if (ev.target === renameBackdrop) closeRenameModal();
  });
  byId("renameShowInput").addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") confirmRenameShow();
  });
  byId("confirmRenameShowBtn").addEventListener("click", confirmRenameShow);

  // Delete modal controls
  const confirmBackdrop = byId("confirmModalBackdrop");
  byId("closeConfirmModalBtn").addEventListener("click", closeDeleteModal);
  byId("cancelConfirmModalBtn").addEventListener("click", closeDeleteModal);
  confirmBackdrop.addEventListener("click", (ev) => {
    if (ev.target === confirmBackdrop) closeDeleteModal();
  });
  byId("confirmDeleteShowBtn").addEventListener("click", confirmDeleteShow);
}
