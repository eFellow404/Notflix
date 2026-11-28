import {
  api,
  byId,
  el,
  encodePath,
  formatTime,
  hasValidIntro,
  pluralize,
  showToast,
  withButtonBusy,
} from "./utils.js";
import { compactEpisodeBadge, findGroupKeyForEpisode } from "./seasons.js";
import { getEpisodeById, setLinkedList, state, updateEpisodeInState } from "./state.js";

let callbacks = {
  onBeforeEpisodeSwitch: async () => {},
  onEpisodeSelected: () => {},
  onLinksChanged: () => {},
};

let searchDebounceTimer = null;
let lastSelectorSig = "";

export function resetSidebarFilters() {
  if (searchDebounceTimer) {
    clearTimeout(searchDebounceTimer);
    searchDebounceTimer = null;
  }
  lastSelectorSig = "";
  state.episodeQuery = "";
  state.unwatchedOnly = false;
  state.reorderMode = false;
  byId("episodeSearchInput").value = "";
  const unwatchedBtn = byId("unwatchedFilterBtn");
  unwatchedBtn.classList.remove("active");
  unwatchedBtn.setAttribute("aria-pressed", "false");
  byId("toggleReorderBtn").classList.remove("active-toggle");
}

function syncLinkedListState(
  linkedList,
  { currentId = state.currentEpisode?.id, notifyLinks = false } = {}
) {
  setLinkedList(linkedList);
  const current = getEpisodeById(currentId);
  if (current) {
    state.currentEpisode = current;
    if (notifyLinks) callbacks.onLinksChanged();
  }
}

function buildSelectorSignature(groups, currentGroupKey) {
  const showId = state.activeShow?.id || "";
  const groupSummary = groups
    .map((g) => {
      const watched = g.episodes.filter((e) => e.watched).length;
      return `${g.key}:${g.label}:${g.episodes.length}:${watched}`;
    })
    .join("|");
  return `${showId}#${state.activeGroupKey}#${currentGroupKey}#${groupSummary}`;
}

function renderSeasonSelector(groups, currentGroupKey) {
  const wrap = byId("seasonSelectorWrap");
  const tabsEl = byId("seasonTabs");
  const selectRow = byId("seasonSelectRow");

  if (groups.length <= 1) {
    lastSelectorSig = "";
    wrap.classList.add("hidden");
    return;
  }

  const sig = buildSelectorSignature(groups, currentGroupKey);
  if (sig === lastSelectorSig) {
    wrap.classList.remove("hidden");
    return;
  }
  lastSelectorSig = sig;

  wrap.classList.remove("hidden");
  if (groups.length <= 5) {
    tabsEl.classList.remove("hidden");
    selectRow.classList.add("hidden");
    const frag = document.createDocumentFragment();
    for (const g of groups) {
      const isActive = g.key === state.activeGroupKey;
      const hasPlaying = g.key === currentGroupKey;
      const classes = ["season-tab"];
      if (isActive) classes.push("active");
      if (hasPlaying) classes.push("has-playing");

      frag.appendChild(
        el(
          "button",
          {
            className: classes.join(" "),
            role: "tab",
            "aria-selected": isActive ? "true" : "false",
            dataset: { seasonKey: g.key },
            type: "button",
          },
          el("span", null, g.label),
          el("span", { className: "season-tab-count" }, g.episodes.length)
        )
      );
    }
    tabsEl.replaceChildren(frag);
  } else {
    tabsEl.classList.add("hidden");
    selectRow.classList.remove("hidden");
    const selectEl = byId("seasonSelect");
    const frag = document.createDocumentFragment();
    for (const g of groups) {
      const watched = g.episodes.filter((e) => e.watched).length;
      const mark = g.key === currentGroupKey ? "● " : "";
      frag.appendChild(
        el(
          "option",
          {
            value: g.key,
            selected: g.key === state.activeGroupKey,
          },
          `${mark}${g.label} (${watched}/${g.episodes.length})`
        )
      );
    }
    selectEl.replaceChildren(frag);
    const curIdx = groups.findIndex((g) => g.key === state.activeGroupKey);
    byId("prevSeasonBtn").disabled = curIdx <= 0;
    byId("nextSeasonBtn").disabled = curIdx < 0 || curIdx >= groups.length - 1;
  }
}

function renderSeasonProgress(activeGroup, currentGroupKey, isSearching) {
  const bar = byId("seasonProgressBar");
  if (!activeGroup || isSearching) {
    bar.classList.add("hidden");
    return;
  }

  const gEps = activeGroup.episodes;
  const gWatched = gEps.filter((e) => e.watched).length;
  const gPct = gEps.length > 0 ? Math.round((gWatched / gEps.length) * 100) : 0;
  const allWatched = gEps.length > 0 && gWatched === gEps.length;

  bar.classList.remove("hidden");
  byId("seasonProgressText").textContent = `${activeGroup.label} · ${gWatched} of ${gEps.length} watched`;
  const fill = byId("seasonProgressFill");
  fill.style.width = `${gPct}%`;
  fill.classList.toggle("complete", allWatched);
  byId("markSeasonWatchedBtn").textContent = allWatched ? "Mark unwatched" : "Mark all watched";

  const jumpBtn = byId("jumpToCurrentBtn");
  if (state.currentEpisode && currentGroupKey && currentGroupKey !== state.activeGroupKey) {
    jumpBtn.textContent = `Jump to ${state.currentEpisode.display_code}`;
    jumpBtn.classList.remove("hidden");
  } else {
    jumpBtn.classList.add("hidden");
  }
}

function formatEpisodeMetaTime(ep) {
  if (ep.watched) {
    return ep.duration_sec > 0 ? `${formatTime(ep.duration_sec)} · Watched` : "Watched";
  }
  if (ep.position_sec > 2) {
    return `${formatTime(ep.position_sec)}${ep.duration_sec > 0 ? ` / ${formatTime(ep.duration_sec)}` : ""}`;
  }
  return ep.duration_sec > 0 ? formatTime(ep.duration_sec) : "Unwatched";
}

function createEpisodeThumbnail(ep) {
  const img = el("img", {
    className: "ep-thumb",
    loading: "lazy",
    alt: "",
  });
  img.addEventListener("load", () => img.classList.add("loaded"), { once: true });
  img.addEventListener("error", () => img.remove(), { once: true });
  img.src = `/api/episodes/${encodePath(ep.id)}/thumbnail`;
  if (img.complete && img.naturalWidth > 0) {
    img.classList.add("loaded");
  }
  return img;
}

function renderEpisodeList(activeGroup, activeId, isSearching, { scrollToActive = false } = {}) {
  const allEps = state.linkedList?.episodes || [];
  const query = state.episodeQuery.trim().toLowerCase();

  let visibleEps = isSearching
    ? allEps.filter((ep) =>
        `${ep.display_code} ${ep.title} ${ep.filename}`.toLowerCase().includes(query)
      )
    : activeGroup
    ? activeGroup.episodes
    : allEps;

  if (state.unwatchedOnly) {
    visibleEps = visibleEps.filter((ep) => !ep.watched);
  }

  const episodeList = byId("episodeList");
  if (visibleEps.length === 0) {
    const emptyText = isSearching
      ? "No episodes match your search."
      : state.unwatchedOnly
      ? "All episodes here are watched!"
      : "No episodes in this group.";
    episodeList.replaceChildren(el("div", { className: "ep-empty-msg" }, emptyText));
    return;
  }

  const allowReorder =
    state.reorderMode && !isSearching && !state.unwatchedOnly && visibleEps.length > 1;
  const showDiscDividers = Boolean(activeGroup?.showDiscDividers && !isSearching);
  let lastDisc = null;

  const frag = document.createDocumentFragment();
  for (let localIdx = 0; localIdx < visibleEps.length; localIdx++) {
    const ep = visibleEps[localIdx];
    const isActive = ep.id === activeId;
    const code = compactEpisodeBadge(ep, activeGroup?.kind || "flat", isSearching);

    if (showDiscDividers && ep.disc !== null && ep.disc !== undefined && ep.disc !== lastDisc) {
      lastDisc = ep.disc;
      frag.appendChild(el("div", { className: "disc-divider" }, `Disc ${ep.disc}`));
    }

    const introDot = hasValidIntro(ep)
      ? el(
          "span",
          {
            className: "ep-intro-dot",
            title: `Intro ${formatTime(ep.intro_start_sec)}–${formatTime(ep.intro_end_sec)}`,
          },
          "Intro"
        )
      : null;

    const extraContext =
      ep.is_extra && ep.season !== null && ep.season !== undefined
        ? el("span", null, `S${String(ep.season).padStart(2, "0")}`)
        : null;

    const showProgress = isActive || (ep.progress_percent > 0 && ep.progress_percent < 100);

    const rightControls = allowReorder
      ? el(
          "div",
          { className: "ep-row-actions" },
          el(
            "button",
            {
              className: "reorder-btn",
              type: "button",
              title: "Move earlier",
              disabled: localIdx === 0,
              dataset: {
                moveEp: ep.id,
                targetIdx: localIdx > 0 ? visibleEps[localIdx - 1].index : ep.index,
              },
            },
            "↑"
          ),
          el(
            "button",
            {
              className: "reorder-btn",
              type: "button",
              title: "Move later",
              disabled: localIdx === visibleEps.length - 1,
              dataset: {
                moveEp: ep.id,
                targetIdx:
                  localIdx < visibleEps.length - 1 ? visibleEps[localIdx + 1].index : ep.index,
              },
            },
            "↓"
          )
        )
      : el(
          "div",
          { className: "ep-row-actions" },
          el(
            "button",
            {
              className: ep.watched ? "watched-toggle-btn is-watched" : "watched-toggle-btn",
              type: "button",
              dataset: { toggleWatched: ep.id },
              title: ep.watched ? "Mark as unwatched" : "Mark as watched",
              "aria-label": ep.watched ? "Mark as unwatched" : "Mark as watched",
            },
            "✓"
          )
        );

    const cardClasses = ["ep-row-card"];
    if (isActive) cardClasses.push("active");
    if (ep.watched) cardClasses.push("watched");

    const card = el(
      "div",
      {
        className: cardClasses.join(" "),
        dataset: { selectEp: ep.id },
      },
      el(
        "div",
        { className: "ep-row-main" },
        createEpisodeThumbnail(ep),
        el(
          "span",
          { className: ep.is_extra ? "ep-code-pill extra" : "ep-code-pill" },
          `${isActive ? "▶ " : ""}${code}`
        ),
        el(
          "div",
          { className: "ep-row-body" },
          el("div", { className: "ep-row-title", title: ep.filename }, ep.title),
          el(
            "div",
            { className: "ep-row-meta" },
            el("span", null, formatEpisodeMetaTime(ep)),
            extraContext,
            introDot
          )
        ),
        rightControls
      ),
      showProgress
        ? el(
            "div",
            { className: "progress-track" },
            el("div", {
              className: ep.watched ? "progress-fill complete" : "progress-fill",
              dataset: { epProgress: ep.id },
              style: { width: `${ep.progress_percent}%` },
            })
          )
        : null
    );

    frag.appendChild(card);
  }

  episodeList.replaceChildren(frag);

  if (scrollToActive) {
    const activeEl = episodeList.querySelector(".ep-row-card.active");
    if (activeEl) {
      activeEl.scrollIntoView({ block: "nearest", behavior: "smooth" });
    }
  }
}

function renderFilteredEpisodeList() {
  if (!state.linkedList || !state.activeShow) return;
  const groups = state.episodeGroups;
  const activeId = state.currentEpisode ? state.currentEpisode.id : state.linkedList.current_id;
  const currentGroupKey = findGroupKeyForEpisode(groups, activeId);
  const isSearching = state.episodeQuery.trim().length > 0;
  const activeGroup = groups.find((g) => g.key === state.activeGroupKey) || groups[0] || null;

  renderSeasonProgress(activeGroup, currentGroupKey, isSearching);
  renderEpisodeList(activeGroup, activeId, isSearching);
}

export function renderSeasonSidebar({ scrollToActive = false } = {}) {
  if (!state.linkedList || !state.activeShow) return;
  const allEps = state.linkedList.episodes || [];
  const groups = state.episodeGroups;

  if (!groups.some((g) => g.key === state.activeGroupKey)) {
    state.activeGroupKey = findGroupKeyForEpisode(groups, state.currentEpisode?.id);
  }

  const totalWatched = allEps.filter((e) => e.watched).length;
  const seasonCount = groups.filter((g) => g.kind === "season").length;
  const summaryParts = [];
  if (seasonCount > 1) summaryParts.push(pluralize(seasonCount, "season"));
  summaryParts.push(pluralize(allEps.length, "episode"));
  summaryParts.push(`${totalWatched} watched`);
  byId("sidebarSummary").textContent = summaryParts.join(" · ");

  // Reorder banner visibility
  const reorderBanner = byId("reorderBanner");
  const toggleReorderBtn = byId("toggleReorderBtn");
  if (state.reorderMode) {
    reorderBanner.classList.remove("hidden");
    toggleReorderBtn.classList.add("active-toggle");
    byId("resetSmartSortBtn").classList.toggle("hidden", !state.activeShow.manual_order);
  } else {
    reorderBanner.classList.add("hidden");
    toggleReorderBtn.classList.remove("active-toggle");
  }

  const activeId = state.currentEpisode ? state.currentEpisode.id : state.linkedList.current_id;
  const currentGroupKey = findGroupKeyForEpisode(groups, activeId);
  const isSearching = state.episodeQuery.trim().length > 0;

  renderSeasonSelector(groups, currentGroupKey);

  const activeGroup = groups.find((g) => g.key === state.activeGroupKey) || groups[0] || null;
  renderSeasonProgress(activeGroup, currentGroupKey, isSearching);
  renderEpisodeList(activeGroup, activeId, isSearching, { scrollToActive });
}

function applyRescanResult(res) {
  state.activeShow = res.show;
  syncLinkedListState(res.linked_list, {
    currentId: res.linked_list?.current_id,
    notifyLinks: true,
  });
  renderSeasonSidebar();
}

export function initSidebar(hooks = {}) {
  callbacks = { ...callbacks, ...hooks };

  // Delegated season tab clicks
  byId("seasonTabs").addEventListener("click", (ev) => {
    const btn = ev.target.closest("[data-season-key]");
    if (!btn) return;
    state.activeGroupKey = btn.dataset.seasonKey;
    renderSeasonSidebar();
  });

  // Delegated episode list clicks (watched toggle, reorder arrows, episode selection)
  byId("episodeList").addEventListener("click", async (ev) => {
    const toggleBtn = ev.target.closest("[data-toggle-watched]");
    if (toggleBtn) {
      ev.stopPropagation();
      const epId = toggleBtn.dataset.toggleWatched;
      const ep = getEpisodeById(epId);
      if (!ep) return;
      const nextWatched = !ep.watched;
      try {
        const res = await api(`/api/episodes/${encodePath(epId)}/progress`, {
          method: "PATCH",
          body: JSON.stringify({
            watched: nextWatched,
            set_current: false,
            reset_position: !nextWatched,
          }),
        });
        updateEpisodeInState(res.episode);
        renderSeasonSidebar();
      } catch (err) {
        showToast(`Could not update watched status: ${err.message}`);
      }
      return;
    }

    const moveBtn = ev.target.closest("[data-move-ep]");
    if (moveBtn) {
      ev.stopPropagation();
      if (moveBtn.disabled) return;
      const epId = moveBtn.dataset.moveEp;
      const targetIdx = Number(moveBtn.dataset.targetIdx);
      try {
        const res = await api(`/api/shows/${encodePath(state.activeShow.id)}/reorder`, {
          method: "POST",
          body: JSON.stringify({ episode_id: epId, target_index: targetIdx }),
        });
        state.activeShow.manual_order = true;
        syncLinkedListState(res.linked_list, { notifyLinks: true });
        renderSeasonSidebar();
      } catch (err) {
        showToast(`Reorder failed: ${err.message}`);
      }
      return;
    }

    const card = ev.target.closest("[data-select-ep]");
    if (!card) return;
    const epId = card.dataset.selectEp;
    if (state.currentEpisode && state.currentEpisode.id === epId) return;
    await callbacks.onBeforeEpisodeSwitch();
    try {
      const res = await api(`/api/shows/${encodePath(state.activeShow.id)}/select`, {
        method: "POST",
        body: JSON.stringify({ episode_id: epId }),
      });
      setLinkedList(res.linked_list);
      callbacks.onEpisodeSelected(res.episode);
    } catch (err) {
      showToast(`Failed to switch episode: ${err.message}`);
    }
  });

  // Season dropdown & prev/next arrows
  const seasonSelect = byId("seasonSelect");
  seasonSelect.addEventListener("change", () => {
    state.activeGroupKey = seasonSelect.value;
    renderSeasonSidebar();
  });

  byId("prevSeasonBtn").addEventListener("click", () => {
    const idx = state.episodeGroups.findIndex((g) => g.key === state.activeGroupKey);
    if (idx > 0) {
      state.activeGroupKey = state.episodeGroups[idx - 1].key;
      renderSeasonSidebar();
    }
  });

  byId("nextSeasonBtn").addEventListener("click", () => {
    const idx = state.episodeGroups.findIndex((g) => g.key === state.activeGroupKey);
    if (idx >= 0 && idx < state.episodeGroups.length - 1) {
      state.activeGroupKey = state.episodeGroups[idx + 1].key;
      renderSeasonSidebar();
    }
  });

  byId("jumpToCurrentBtn").addEventListener("click", () => {
    if (!state.currentEpisode) return;
    state.activeGroupKey = findGroupKeyForEpisode(state.episodeGroups, state.currentEpisode.id);
    renderSeasonSidebar({ scrollToActive: true });
  });

  byId("markSeasonWatchedBtn").addEventListener("click", async () => {
    if (!state.activeShow) return;
    const activeGroup = state.episodeGroups.find((g) => g.key === state.activeGroupKey);
    if (!activeGroup || !activeGroup.episodes.length) return;
    const allWatched = activeGroup.episodes.every((e) => e.watched);
    const nextWatched = !allWatched;
    try {
      const res = await api(`/api/shows/${encodePath(state.activeShow.id)}/watched`, {
        method: "POST",
        body: JSON.stringify({
          episode_ids: activeGroup.episodes.map((e) => e.id),
          watched: nextWatched,
          reset_position: !nextWatched,
        }),
      });
      syncLinkedListState(res.linked_list);
      renderSeasonSidebar();
      showToast(
        `${activeGroup.label}: marked ${nextWatched ? "all watched" : "all unwatched"}`,
        "success"
      );
    } catch (err) {
      showToast(`Could not update season: ${err.message}`);
    }
  });

  const episodeSearchInput = byId("episodeSearchInput");
  episodeSearchInput.addEventListener("input", () => {
    state.episodeQuery = episodeSearchInput.value;
    clearTimeout(searchDebounceTimer);
    searchDebounceTimer = setTimeout(() => {
      searchDebounceTimer = null;
      renderFilteredEpisodeList();
    }, 80);
  });

  const unwatchedFilterBtn = byId("unwatchedFilterBtn");
  unwatchedFilterBtn.addEventListener("click", () => {
    state.unwatchedOnly = !state.unwatchedOnly;
    unwatchedFilterBtn.classList.toggle("active", state.unwatchedOnly);
    unwatchedFilterBtn.setAttribute("aria-pressed", String(state.unwatchedOnly));
    renderFilteredEpisodeList();
  });

  byId("toggleReorderBtn").addEventListener("click", () => {
    state.reorderMode = !state.reorderMode;
    renderSeasonSidebar();
  });

  byId("doneReorderBtn").addEventListener("click", () => {
    state.reorderMode = false;
    renderSeasonSidebar();
  });

  const rescanShowBtn = byId("rescanShowBtn");
  rescanShowBtn.addEventListener("click", async () => {
    if (!state.activeShow) return;
    await withButtonBusy(rescanShowBtn, "Scanning…", async () => {
      try {
        const res = await api(`/api/shows/${encodePath(state.activeShow.id)}/rescan`, {
          method: "POST",
          body: JSON.stringify({ reset_order: false }),
        });
        applyRescanResult(res);
        showToast(`Rescanned: +${res.diff.added} added, -${res.diff.removed} removed`, "success");
      } catch (err) {
        showToast(`Rescan failed: ${err.message}`);
      }
    });
  });

  const resetSmartSortBtn = byId("resetSmartSortBtn");
  resetSmartSortBtn.addEventListener("click", async () => {
    if (!state.activeShow) return;
    await withButtonBusy(resetSmartSortBtn, "Resetting…", async () => {
      try {
        const res = await api(`/api/shows/${encodePath(state.activeShow.id)}/rescan`, {
          method: "POST",
          body: JSON.stringify({ reset_order: true }),
        });
        applyRescanResult(res);
        showToast("Restored automatic episode order", "success");
      } catch (err) {
        showToast(`Reset failed: ${err.message}`);
      }
    });
  });
}
