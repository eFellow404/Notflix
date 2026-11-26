import { api, byId, escapeHtml, formatTime, hasValidIntro, showToast } from "./utils.js";
import { compactEpisodeBadge, findGroupKeyForEpisode } from "./seasons.js";
import { getEpisodeById, setLinkedList, state, updateEpisodeInState } from "./state.js";

let callbacks = {
  onBeforeEpisodeSwitch: async () => {},
  onEpisodeSelected: () => {},
  onLinksChanged: () => {},
};

export function resetSidebarFilters() {
  state.episodeQuery = "";
  state.unwatchedOnly = false;
  state.reorderMode = false;
  byId("episodeSearchInput").value = "";
  const unwatchedBtn = byId("unwatchedFilterBtn");
  unwatchedBtn.classList.remove("active");
  unwatchedBtn.setAttribute("aria-pressed", "false");
  byId("toggleReorderBtn").classList.remove("active-toggle");
}

function renderSeasonSelector(groups, currentGroupKey) {
  const wrap = byId("seasonSelectorWrap");
  const tabsEl = byId("seasonTabs");
  const selectRow = byId("seasonSelectRow");

  if (groups.length <= 1) {
    wrap.classList.add("hidden");
    return;
  }

  wrap.classList.remove("hidden");
  if (groups.length <= 5) {
    tabsEl.classList.remove("hidden");
    selectRow.classList.add("hidden");
    tabsEl.innerHTML = groups
      .map((g) => {
        const isActive = g.key === state.activeGroupKey;
        const hasPlaying = g.key === currentGroupKey;
        return `
          <button
            class="season-tab ${isActive ? "active" : ""} ${hasPlaying ? "has-playing" : ""}"
            role="tab"
            aria-selected="${isActive ? "true" : "false"}"
            data-season-key="${escapeHtml(g.key)}"
            type="button"
          >
            <span>${escapeHtml(g.label)}</span>
            <span class="season-tab-count">${g.episodes.length}</span>
          </button>
        `;
      })
      .join("");
  } else {
    tabsEl.classList.add("hidden");
    selectRow.classList.remove("hidden");
    byId("seasonSelect").innerHTML = groups
      .map((g) => {
        const watched = g.episodes.filter((e) => e.watched).length;
        const mark = g.key === currentGroupKey ? "● " : "";
        return `<option value="${escapeHtml(g.key)}" ${g.key === state.activeGroupKey ? "selected" : ""}>${mark}${escapeHtml(g.label)} (${watched}/${g.episodes.length})</option>`;
      })
      .join("");
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
  if (seasonCount > 1) summaryParts.push(`${seasonCount} seasons`);
  summaryParts.push(`${allEps.length} episode${allEps.length === 1 ? "" : "s"}`);
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
  const query = state.episodeQuery.trim().toLowerCase();
  const isSearching = query.length > 0;

  renderSeasonSelector(groups, currentGroupKey);

  const activeGroup = groups.find((g) => g.key === state.activeGroupKey) || groups[0] || null;
  renderSeasonProgress(activeGroup, currentGroupKey, isSearching);

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
    episodeList.innerHTML = `<div class="ep-empty-msg">${
      isSearching
        ? "No episodes match your search."
        : state.unwatchedOnly
        ? "All episodes here are watched!"
        : "No episodes in this group."
    }</div>`;
    return;
  }

  const allowReorder = state.reorderMode && !isSearching && !state.unwatchedOnly && visibleEps.length > 1;
  const showDiscDividers = Boolean(activeGroup?.showDiscDividers && !isSearching);
  let lastDisc = null;

  episodeList.innerHTML = visibleEps
    .map((ep, localIdx) => {
      const isActive = ep.id === activeId;
      const code = compactEpisodeBadge(ep, activeGroup?.kind || "flat", isSearching);
      let dividerHtml = "";
      if (showDiscDividers && ep.disc !== null && ep.disc !== undefined && ep.disc !== lastDisc) {
        lastDisc = ep.disc;
        dividerHtml = `<div class="disc-divider">Disc ${ep.disc}</div>`;
      }

      const introDotHtml = hasValidIntro(ep)
        ? `<span class="ep-intro-dot" title="Intro ${formatTime(ep.intro_start_sec)}–${formatTime(ep.intro_end_sec)}">Intro</span>`
        : "";
      const extraContext =
        ep.is_extra && ep.season !== null && ep.season !== undefined
          ? `<span>S${String(ep.season).padStart(2, "0")}</span>`
          : "";
      const showProgress = isActive || (ep.progress_percent > 0 && ep.progress_percent < 100);

      const rightControls = allowReorder
        ? `
          <div class="ep-row-actions">
            <button class="reorder-btn" data-move-ep="${ep.id}" data-target-idx="${localIdx > 0 ? visibleEps[localIdx - 1].index : ep.index}" type="button" title="Move earlier" ${localIdx === 0 ? "disabled" : ""}>↑</button>
            <button class="reorder-btn" data-move-ep="${ep.id}" data-target-idx="${localIdx < visibleEps.length - 1 ? visibleEps[localIdx + 1].index : ep.index}" type="button" title="Move later" ${localIdx === visibleEps.length - 1 ? "disabled" : ""}>↓</button>
          </div>
        `
        : `
          <div class="ep-row-actions">
            <button
              class="watched-toggle-btn ${ep.watched ? "is-watched" : ""}"
              data-toggle-watched="${ep.id}"
              type="button"
              title="${ep.watched ? "Mark as unwatched" : "Mark as watched"}"
              aria-label="${ep.watched ? "Mark as unwatched" : "Mark as watched"}"
            >✓</button>
          </div>
        `;

      return `
        ${dividerHtml}
        <div class="ep-row-card ${isActive ? "active" : ""} ${ep.watched ? "watched" : ""}" data-select-ep="${ep.id}">
          <div class="ep-row-main">
            <span class="ep-code-pill ${ep.is_extra ? "extra" : ""}">${isActive ? "▶ " : ""}${escapeHtml(code)}</span>
            <div class="ep-row-body">
              <div class="ep-row-title" title="${escapeHtml(ep.filename)}">${escapeHtml(ep.title)}</div>
              <div class="ep-row-meta">
                <span>${escapeHtml(formatEpisodeMetaTime(ep))}</span>
                ${extraContext}
                ${introDotHtml}
              </div>
            </div>
            ${rightControls}
          </div>
          ${
            showProgress
              ? `<div class="progress-track"><div class="progress-fill ${ep.watched ? "complete" : ""}" data-ep-progress="${ep.id}" style="width: ${ep.progress_percent}%"></div></div>`
              : ""
          }
        </div>
      `;
    })
    .join("");

  if (scrollToActive) {
    const activeEl = episodeList.querySelector(".ep-row-card.active");
    if (activeEl) {
      activeEl.scrollIntoView({ block: "nearest", behavior: "smooth" });
    }
  }
}

function applyRescanResult(res) {
  state.activeShow = res.show;
  setLinkedList(res.linked_list);
  const current = getEpisodeById(state.linkedList.current_id);
  if (current) {
    state.currentEpisode = current;
    callbacks.onLinksChanged();
  }
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
        const res = await api(`/api/episodes/${epId}/progress`, {
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
        const res = await api(`/api/shows/${state.activeShow.id}/reorder`, {
          method: "POST",
          body: JSON.stringify({ episode_id: epId, target_index: targetIdx }),
        });
        state.activeShow.manual_order = true;
        setLinkedList(res.linked_list);
        const refreshed = getEpisodeById(state.currentEpisode?.id);
        if (refreshed) {
          state.currentEpisode = refreshed;
          callbacks.onLinksChanged();
        }
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
      const res = await api(`/api/shows/${state.activeShow.id}/select`, {
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
      const res = await api(`/api/shows/${state.activeShow.id}/watched`, {
        method: "POST",
        body: JSON.stringify({
          episode_ids: activeGroup.episodes.map((e) => e.id),
          watched: nextWatched,
          reset_position: !nextWatched,
        }),
      });
      setLinkedList(res.linked_list);
      const refreshed = getEpisodeById(state.currentEpisode?.id);
      if (refreshed) state.currentEpisode = refreshed;
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
    renderSeasonSidebar();
  });

  const unwatchedFilterBtn = byId("unwatchedFilterBtn");
  unwatchedFilterBtn.addEventListener("click", () => {
    state.unwatchedOnly = !state.unwatchedOnly;
    unwatchedFilterBtn.classList.toggle("active", state.unwatchedOnly);
    unwatchedFilterBtn.setAttribute("aria-pressed", String(state.unwatchedOnly));
    renderSeasonSidebar();
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
    rescanShowBtn.disabled = true;
    rescanShowBtn.textContent = "Scanning…";
    try {
      const res = await api(`/api/shows/${state.activeShow.id}/rescan`, {
        method: "POST",
        body: JSON.stringify({ reset_order: false }),
      });
      applyRescanResult(res);
      showToast(`Rescanned: +${res.diff.added} added, -${res.diff.removed} removed`, "success");
    } catch (err) {
      showToast(`Rescan failed: ${err.message}`);
    } finally {
      rescanShowBtn.disabled = false;
      rescanShowBtn.textContent = "↻ Rescan";
    }
  });

  byId("resetSmartSortBtn").addEventListener("click", async () => {
    if (!state.activeShow) return;
    try {
      const res = await api(`/api/shows/${state.activeShow.id}/rescan`, {
        method: "POST",
        body: JSON.stringify({ reset_order: true }),
      });
      applyRescanResult(res);
      showToast("Restored automatic episode order", "success");
    } catch (err) {
      showToast(`Reset failed: ${err.message}`);
    }
  });
}
