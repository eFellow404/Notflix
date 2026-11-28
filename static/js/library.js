import {
  api,
  byId,
  el,
  encodePath,
  formatTime,
  parseErrorResponse,
  pluralize,
  showMonogram,
  showPosterStyle,
  showToast,
  withButtonBusy,
} from "./utils.js";
import { state } from "./state.js";
import {
  closeOpenModal,
  initLibraryModals,
  isAnyModalOpen,
  openAddModal,
  openDeleteModal,
  openRenameModal,
} from "./library-modals.js";

export { closeOpenModal, isAnyModalOpen, openAddModal };

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
    parts.push(pluralize(show.season_count, "season"));
  } else if (show.disc_count > 0) {
    parts.push(pluralize(show.disc_count, "disc"));
  }
  const mainCount = show.main_episode_count ?? show.episode_count;
  parts.push(pluralize(mainCount, "episode"));
  if (show.extra_count > 0) {
    parts.push(`+${pluralize(show.extra_count, "extra")}`);
  }
  return parts.join(" · ");
}

function createPosterArt(show, containerClass, attrs = {}, ...extraChildren) {
  let posterImg = null;
  if (show.has_poster) {
    posterImg = el("img", {
      className: "show-poster-img",
      src: `/api/shows/${encodePath(show.id)}/poster`,
      alt: "",
      loading: "lazy",
    });
    posterImg.addEventListener("error", () => posterImg.remove(), { once: true });
  }
  return el(
    "div",
    {
      ...attrs,
      className: containerClass,
      style: { background: showPosterStyle(show.name), ...(attrs.style || {}) },
    },
    el("span", { className: "show-poster-monogram" }, showMonogram(show.name)),
    posterImg,
    ...extraChildren
  );
}

function renderContinueHero(allShows) {
  const continueHero = byId("continueHero");
  const heroShow = allShows.find((s) => s.current_episode) || allShows[0];
  if (!heroShow || !heroShow.current_episode) {
    continueHero.classList.add("hidden");
    continueHero.replaceChildren();
    return;
  }

  const ep = heroShow.current_episode;
  const nxt = heroShow.next_episode;
  const timeLabel =
    ep.position_sec > 2
      ? `Paused at ${formatTime(ep.position_sec)}${ep.duration_sec > 0 ? ` / ${formatTime(ep.duration_sec)}` : ""}`
      : "Ready to start";
  const seasonInfo = heroShow.season_count > 0 ? `${pluralize(heroShow.season_count, "season")} · ` : "";

  let backdrop = null;
  if (heroShow.has_poster) {
    const img = el("img", {
      src: `/api/shows/${encodePath(heroShow.id)}/poster`,
      alt: "",
    });
    backdrop = el("div", { className: "hero-backdrop", "aria-hidden": "true" }, img);
    img.addEventListener("error", () => backdrop.remove(), { once: true });
  }

  const frag = document.createDocumentFragment();
  if (backdrop) frag.appendChild(backdrop);

  frag.appendChild(
    el(
      "div",
      { className: "hero-content" },
      el("div", { className: "hero-eyebrow" }, heroShow.last_watched_at ? "Continue Watching" : "Ready To Watch"),
      el("h2", null, heroShow.name),
      el(
        "div",
        { className: "hero-ep-line" },
        el("span", { className: "ep-code-badge" }, ep.display_code),
        el("strong", null, ep.title),
        el("span", { className: "muted" }, `· ${timeLabel}`)
      ),
      el(
        "div",
        { className: "hero-progress-wrap" },
        el(
          "div",
          { className: "progress-track" },
          el("div", { className: "progress-fill", style: { width: `${ep.progress_percent}%` } })
        ),
        el("div", { className: "hero-substats" }, `${seasonInfo}${heroShow.watched_count} of ${heroShow.episode_count} watched`)
      )
    )
  );

  frag.appendChild(
    el(
      "div",
      { className: "hero-actions" },
      el(
        "button",
        {
          className: "btn btn-primary btn-lg",
          type: "button",
          dataset: { heroWatch: heroShow.id },
        },
        `▶ ${ep.position_sec > 2 ? `Resume (${formatTime(ep.position_sec)})` : "Watch Now"}`
      ),
      nxt
        ? el(
            "button",
            {
              className: "btn btn-secondary btn-lg",
              type: "button",
              dataset: { heroNext: heroShow.id },
            },
            `Next: ${nxt.display_code} →`
          )
        : null
    )
  );

  continueHero.classList.remove("hidden");
  continueHero.replaceChildren(frag);
}

function renderContinueStrip(allShows) {
  const section = byId("continueSection");
  const strip = byId("continueStrip");
  if (!section || !strip) return;

  const inProgress = allShows
    .filter((s) => s.last_watched_at && s.watched_count < s.episode_count && s.current_episode)
    .sort((a, b) => String(b.last_watched_at).localeCompare(String(a.last_watched_at)));

  if (inProgress.length < 2) {
    section.classList.add("hidden");
    strip.replaceChildren();
    return;
  }

  const frag = document.createDocumentFragment();
  for (const show of inProgress) {
    const ep = show.current_episode;
    const resumeMeta = ep.position_sec > 2 ? formatTime(ep.position_sec) : "Up next";
    const art = createPosterArt(
      show,
      "continue-card-art",
      {},
      el("span", { className: "continue-card-play", "aria-hidden": "true" }, "▶"),
      el(
        "div",
        { className: "continue-card-progress" },
        el(
          "div",
          { className: "progress-track" },
          el("div", { className: "progress-fill", style: { width: `${ep.progress_percent}%` } })
        )
      )
    );

    frag.appendChild(
      el(
        "button",
        {
          className: "continue-card",
          type: "button",
          dataset: { continueShow: show.id },
          title: `Resume ${show.name} · ${ep.display_code}`,
        },
        art,
        el(
          "div",
          { className: "continue-card-body" },
          el("div", { className: "continue-card-title" }, show.name),
          el(
            "div",
            { className: "continue-card-meta" },
            el("span", { className: "continue-card-code" }, ep.display_code),
            el("span", { className: "muted" }, `· ${resumeMeta}`)
          )
        )
      )
    );
  }

  section.classList.remove("hidden");
  strip.replaceChildren(frag);
}

export function renderShowsGrid() {
  const showsGrid = byId("showsGrid");
  const libraryNoResults = byId("libraryNoResults");

  const shows = getFilteredSortedShows();
  if (shows.length === 0) {
    showsGrid.replaceChildren();
    libraryNoResults.classList.remove("hidden");
    return;
  }
  libraryNoResults.classList.add("hidden");

  const frag = document.createDocumentFragment();
  for (const show of shows) {
    const ep = show.current_episode;
    const pctShow =
      show.episode_count > 0 ? Math.round((show.watched_count / show.episode_count) * 100) : 0;

    const epInfoNode = ep
      ? el(
          "div",
          {
            className: "show-current-box",
            dataset: { openShow: show.id },
            title: "Click to resume",
          },
          el(
            "div",
            { className: "show-current-row" },
            el("span", { className: "show-current-label" }, el("strong", null, ep.display_code), ` · ${ep.title}`),
            el("span", { className: "muted" }, ep.position_sec > 2 ? formatTime(ep.position_sec) : "Start")
          ),
          el(
            "div",
            { className: "progress-track" },
            el("div", {
              className: ep.watched ? "progress-fill complete" : "progress-fill",
              style: { width: `${ep.progress_percent}%` },
            })
          )
        )
      : el("div", { className: "show-current-box muted" }, "No video files found in this folder yet.");

    const card = el(
      "article",
      { className: "show-card" },
      el(
        "div",
        { className: "show-card-top" },
        createPosterArt(show, "show-poster", {
          dataset: { openShow: show.id },
          "aria-hidden": "true",
        }),
        el(
          "div",
          { className: "show-card-meta" },
          el(
            "div",
            { className: "show-card-title-row" },
            el(
              "h3",
              {
                className: "show-card-title",
                dataset: { openShow: show.id },
                title: show.name,
              },
              show.name
            ),
            el(
              "button",
              {
                className: "icon-btn",
                type: "button",
                dataset: { renameShow: show.id },
                title: "Rename show",
                "aria-label": `Rename ${show.name}`,
              },
              "✎"
            )
          ),
          el("div", { className: "show-card-path", title: show.folder_path }, show.folder_path)
        )
      ),
      el(
        "div",
        { className: "show-stats" },
        el("span", { className: "pill" }, formatShowCountPill(show)),
        el("span", { className: "pill" }, `${show.watched_count}/${show.episode_count} watched (${pctShow}%)`),
        show.manual_order ? el("span", { className: "pill pill-accent" }, "Custom Order") : null,
        !show.folder_exists ? el("span", { className: "pill pill-warn" }, "Folder Missing") : null
      ),
      epInfoNode,
      el(
        "div",
        { className: "show-card-actions" },
        el(
          "button",
          {
            className: "btn btn-primary btn-sm",
            type: "button",
            dataset: { openShow: show.id },
            disabled: show.episode_count === 0,
          },
          `▶ ${ep && ep.position_sec > 2 ? "Resume" : "Watch"}`
        ),
        el(
          "button",
          {
            className: "btn btn-secondary btn-sm",
            type: "button",
            dataset: { rescanShow: show.id },
            title: "Scan folder for added or removed episodes",
          },
          "↻ Rescan"
        ),
        el(
          "button",
          {
            className: "btn btn-danger btn-sm",
            type: "button",
            dataset: { deleteShow: show.id },
            title: "Remove from Notflix (keeps files on disk)",
            "aria-label": `Remove ${show.name}`,
          },
          "✕"
        )
      )
    );

    frag.appendChild(card);
  }

  showsGrid.replaceChildren(frag);
}

export function renderLibrary() {
  const allShows = state.shows;
  const showsGrid = byId("showsGrid");
  const libraryNoResults = byId("libraryNoResults");

  if (allShows.length === 0) {
    byId("continueHero").classList.add("hidden");
    byId("continueSection")?.classList.add("hidden");
    byId("libraryHeader").classList.add("hidden");
    libraryNoResults.classList.add("hidden");
    showsGrid.replaceChildren();
    byId("emptyLibrary").classList.remove("hidden");
    return;
  }

  byId("emptyLibrary").classList.add("hidden");
  byId("libraryHeader").classList.remove("hidden");
  byId("libraryToolbar").classList.toggle("hidden", allShows.length < 2);

  renderContinueHero(allShows);
  renderContinueStrip(allShows);
  renderShowsGrid();
}

export function initLibrary(hooks = {}) {
  callbacks = { ...callbacks, ...hooks };

  initLibraryModals({
    onLibraryChanged: () => loadLibrary(),
    onOpenShow: (showId, opts) => callbacks.onOpenShow(showId, opts),
  });

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

  // Delegated Continue Watching strip clicks
  byId("continueStrip")?.addEventListener("click", (ev) => {
    const card = ev.target.closest("[data-continue-show]");
    if (!card) return;
    callbacks.onOpenShow(card.dataset.continueShow, { autoPlay: true });
  });

  // Export & Import watch history
  byId("btnExportLibrary")?.addEventListener("click", async () => {
    try {
      const res = await fetch("/api/library/export");
      if (!res.ok) {
        throw new Error(await parseErrorResponse(res, `Export failed (${res.status})`));
      }
      const blob = await res.blob();
      const url = URL.createObjectURL(blob);
      const link = el("a", { href: url, download: "notflix-watch-history.json" });
      document.body.appendChild(link);
      link.click();
      link.remove();
      setTimeout(() => URL.revokeObjectURL(url), 1000);
      showToast("Exported watch history (notflix-watch-history.json)", "success");
    } catch (err) {
      showToast(`Export failed: ${err.message}`);
    }
  });

  const importInput = byId("importFileInput");
  byId("btnImportLibrary")?.addEventListener("click", () => {
    if (!importInput) return;
    importInput.value = "";
    importInput.click();
  });

  importInput?.addEventListener("change", async () => {
    const file = importInput.files?.[0];
    if (!file) return;
    try {
      const text = await file.text();
      let parsed;
      try {
        parsed = JSON.parse(text);
      } catch {
        throw new Error("Selected file is not valid JSON.");
      }
      const res = await api("/api/library/import", {
        method: "POST",
        body: JSON.stringify(parsed),
      });
      const s = res.summary || {};
      const matchedShows = s.matched_shows ?? s.shows_matched ?? 0;
      const updatedEps = s.updated_episodes ?? s.episodes_updated ?? 0;
      showToast(
        `Imported watch history: ${pluralize(matchedShows, "show")} matched, ${pluralize(updatedEps, "episode")} updated`,
        "success"
      );
      await loadLibrary();
    } catch (err) {
      showToast(`Import failed: ${err.message}`);
    } finally {
      importInput.value = "";
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
      const showId = rescanBtn.dataset.rescanShow;
      await withButtonBusy(rescanBtn, "Scanning…", async () => {
        try {
          const res = await api(`/api/shows/${encodePath(showId)}/rescan`, {
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
        }
      });
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

  // Library search & sort (only re-renders #showsGrid so hero/strip posters stay intact)
  const librarySearchInput = byId("librarySearchInput");
  librarySearchInput.addEventListener("input", () => {
    state.libraryQuery = librarySearchInput.value;
    renderShowsGrid();
  });

  const librarySortSelect = byId("librarySortSelect");
  librarySortSelect.addEventListener("change", () => {
    state.librarySort = librarySortSelect.value;
    renderShowsGrid();
  });

  byId("clearLibrarySearchBtn").addEventListener("click", () => {
    state.libraryQuery = "";
    librarySearchInput.value = "";
    renderShowsGrid();
  });
}
