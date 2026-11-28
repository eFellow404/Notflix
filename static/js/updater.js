import { api, byId, showToast, withButtonBusy } from "./utils.js";

let lastStatus = null;

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
    const label =
      info.behind_count > 0
        ? `↑ Update (${info.behind_count})`
        : info.remote_short_sha
        ? `↑ Update (${info.remote_short_sha})`
        : "↑ Update";
    btn.textContent = label;
    btn.title = `${info.message} Click to back up your database and update.`;
    btn.classList.add("btn-update-available");
    return;
  }

  btn.textContent = "↻ Updates";
  btn.title = info.message || "Check for updates";
}

async function handleUpdateClick() {
  const btn = byId("updateBtn");
  if (!btn) return;

  if (lastStatus?.status === "updated") {
    window.location.reload();
    return;
  }

  const shouldApply = lastStatus?.status === "update_available";
  await withButtonBusy(btn, shouldApply ? "Updating…" : "Checking…", async () => {
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
    }
  });
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
