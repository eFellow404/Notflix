import { api, byId, showToast } from "./utils.js";

let lastStatus = null;
let busy = false;

function renderUpdateButton(info) {
  const btn = byId("updateBtn");
  if (!btn || !info) return;

  btn.classList.remove("btn-update-available");

  if (info.status === "no_git" || info.status === "not_a_repo" || info.status === "no_remote") {
    btn.classList.add("hidden");
    return;
  }

  btn.classList.remove("hidden");

  if (info.status === "updated") {
    btn.textContent = "✓ Updated · Reload";
    btn.title = `${info.message} Restart Notflix to load backend updates, or click to reload the UI.`;
    btn.classList.add("btn-update-available");
    return;
  }

  if (info.status === "update_available") {
    const count = info.behind_count || 1;
    btn.textContent = `↑ Update (${count})`;
    btn.title = `${info.message} Click to back up your database and update.`;
    btn.classList.add("btn-update-available");
    return;
  }

  btn.textContent = "↻ Updates";
  btn.title = info.message || "Check remote Git repository for updates";
}

async function handleUpdateClick() {
  const btn = byId("updateBtn");
  if (!btn || busy) return;

  if (lastStatus?.status === "updated") {
    window.location.reload();
    return;
  }

  busy = true;
  btn.disabled = true;
  const shouldApply = lastStatus?.status === "update_available";
  btn.textContent = shouldApply ? "Updating…" : "Checking…";

  try {
    const endpoint = shouldApply ? "/api/update/apply" : "/api/update/check";
    const res = await api(endpoint, { method: "POST", body: "{}" });
    lastStatus = res;
    renderUpdateButton(res);

    if (res.status === "updated") {
      const backupNote = res.backup_path ? " Database backed up." : "";
      showToast(
        `${res.message}${backupNote} Restart Notflix (or click Reload) to use the latest version.`,
        "success"
      );
    } else if (res.status === "update_available") {
      showToast(`${res.message} Click "↑ Update" to install.`);
    } else if (res.status === "up_to_date") {
      showToast(res.message, "success");
    } else {
      showToast(res.message || "Could not check for updates right now.");
    }
  } catch (err) {
    showToast(`Update check failed: ${err.message}`);
    renderUpdateButton(lastStatus || { status: "idle" });
  } finally {
    busy = false;
    btn.disabled = false;
  }
}

export async function initUpdater() {
  const btn = byId("updateBtn");
  if (!btn) return;

  btn.addEventListener("click", handleUpdateClick);

  try {
    const status = await api("/api/update/status");
    lastStatus = status;
    renderUpdateButton(status);
  } catch {
    // Non-fatal: leave update button ready for manual checks
  }
}
