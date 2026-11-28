import {
  api,
  byId,
  el,
  encodePath,
  folderBasename,
  pluralize,
  showToast,
  withButtonBusy,
} from "./utils.js";
import { state } from "./state.js";

const registeredModals = [];

let modalCallbacks = {
  onLibraryChanged: async () => {},
  onOpenShow: async () => {},
};

let addModalHandle = null;
let renameModalHandle = null;
let deleteModalHandle = null;

export function registerModal({ backdropId, closeIds = [], onClose = null }) {
  const backdrop = byId(backdropId);
  const close = () => {
    backdrop?.classList.add("hidden");
    onClose?.();
  };
  const isOpen = () => Boolean(backdrop && !backdrop.classList.contains("hidden"));

  for (const id of closeIds) {
    byId(id)?.addEventListener("click", close);
  }
  backdrop?.addEventListener("click", (ev) => {
    if (ev.target === backdrop) close();
  });

  const handle = { backdropId, close, isOpen };
  registeredModals.push(handle);
  return handle;
}

export function isAnyModalOpen() {
  return registeredModals.some((m) => m.isOpen());
}

export function closeOpenModal() {
  for (const modal of registeredModals) {
    if (modal.isOpen()) {
      modal.close();
      return true;
    }
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
    browserList.replaceChildren(
      el(
        "div",
        { className: "muted small browser-empty-hint" },
        "No subfolders here. Click the button below to add this folder."
      )
    );
    return;
  }
  if (!dirs.length) {
    browserList.replaceChildren(
      el("div", { className: "muted small browser-empty-hint" }, `No subfolders match "${q}".`)
    );
    return;
  }

  const frag = document.createDocumentFragment();
  for (const dir of dirs) {
    frag.appendChild(
      el(
        "button",
        {
          className: "browser-item",
          type: "button",
          dataset: { dirPath: dir.path },
        },
        el("span", null, "📁"),
        el("span", null, dir.name)
      )
    );
  }
  browserList.replaceChildren(frag);
}

async function browseDirectory(path = "") {
  const addModalError = byId("addModalError");
  const browserList = byId("browserList");
  addModalError.classList.add("hidden");
  browserList.replaceChildren(
    el("div", { className: "muted small browser-empty-hint" }, "Loading folders…")
  );

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
      badge.textContent = `${pluralize(data.direct_video_count, "video")} here`;
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
    browserList.replaceChildren();
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

  addModalError.classList.add("hidden");
  await withButtonBusy(confirmBtn, "Scanning…", async () => {
    try {
      const res = await api("/api/shows", {
        method: "POST",
        body: JSON.stringify({ folder_path: folderPath, name: customName || null }),
      });
      state.lastAddedParent = state.browseParent || "";
      addModalHandle?.close();
      showToast(
        `Added "${res.show.name}" (${pluralize(res.linked_list.length, "episode")})`,
        "success"
      );
      await modalCallbacks.onLibraryChanged();
      if (res.linked_list.length > 0) {
        await modalCallbacks.onOpenShow(res.show.id, { autoPlay: false });
      }
    } catch (err) {
      addModalError.textContent = err.message;
      addModalError.classList.remove("hidden");
    }
  });
  updateAddModalFolderLabels(folderPathInput.value.trim());
}

export function openRenameModal(showId) {
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
  await withButtonBusy(btn, null, async () => {
    try {
      await api(`/api/shows/${encodePath(showId)}`, {
        method: "PATCH",
        body: JSON.stringify({ name: newName }),
      });
      renameModalHandle?.close();
      showToast(`Renamed show to "${newName}"`, "success");
      await modalCallbacks.onLibraryChanged();
    } catch (err) {
      renameModalError.textContent = err.message;
      renameModalError.classList.remove("hidden");
    }
  });
}

export function openDeleteModal(showId) {
  const show = state.shows.find((s) => s.id === showId);
  if (!show) return;
  state.pendingDeleteShowId = showId;
  byId("confirmModalMessage").textContent = `Remove "${show.name}" from your Notflix library?`;
  byId("confirmModalBackdrop").classList.remove("hidden");
  byId("cancelConfirmModalBtn").focus();
}

async function confirmDeleteShow() {
  const showId = state.pendingDeleteShowId;
  if (!showId) return;
  const btn = byId("confirmDeleteShowBtn");
  await withButtonBusy(btn, null, async () => {
    try {
      await api(`/api/shows/${encodePath(showId)}`, { method: "DELETE" });
      deleteModalHandle?.close();
      showToast("Removed show from library");
      await modalCallbacks.onLibraryChanged();
    } catch (err) {
      showToast(`Remove failed: ${err.message}`);
    }
  });
}

export function initLibraryModals(hooks = {}) {
  modalCallbacks = { ...modalCallbacks, ...hooks };

  addModalHandle = registerModal({
    backdropId: "addModalBackdrop",
    closeIds: ["closeAddModalBtn", "cancelAddModalBtn"],
  });

  renameModalHandle = registerModal({
    backdropId: "renameModalBackdrop",
    closeIds: ["closeRenameModalBtn", "cancelRenameModalBtn"],
    onClose: () => {
      state.pendingRenameShowId = null;
    },
  });

  deleteModalHandle = registerModal({
    backdropId: "confirmModalBackdrop",
    closeIds: ["closeConfirmModalBtn", "cancelConfirmModalBtn"],
    onClose: () => {
      state.pendingDeleteShowId = null;
    },
  });

  // Delegated folder browser directory clicks
  byId("browserList").addEventListener("click", (ev) => {
    const btn = ev.target.closest("[data-dir-path]");
    if (!btn) return;
    browseDirectory(btn.dataset.dirPath);
  });

  const folderPathInput = byId("folderPathInput");
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

  byId("renameShowInput").addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") confirmRenameShow();
  });
  byId("confirmRenameShowBtn").addEventListener("click", confirmRenameShow);
  byId("confirmDeleteShowBtn").addEventListener("click", confirmDeleteShow);
}
