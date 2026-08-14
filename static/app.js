const dropEl = document.getElementById("drop");
const fileInput = document.getElementById("fileInput");
const folderInput = document.getElementById("folderInput");
const optionsPanel = document.getElementById("optionsPanel");
const selectedFileName = document.getElementById("selectedFileName");
let selectedFiles = [];
let selectedFolder = false;
let publicBaseUrl = "http://127.0.0.1:7421";
let publicReady = false;

function toast(message) {
  const element = document.getElementById("toast");
  element.textContent = message;
  element.style.display = "block";
  setTimeout(() => { element.style.display = "none"; }, 2500);
}

function humanSize(bytes) {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let index = 0;
  let value = bytes;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index++;
  }
  return value.toFixed(value < 10 && index > 0 ? 1 : 0) + " " + units[index];
}

function pickFiles(files, isFolder) {
  selectedFiles = Array.from(files);
  selectedFolder = isFolder || selectedFiles.length > 1;
  const total = selectedFiles.reduce((sum, file) => sum + file.size, 0);
  selectedFileName.textContent = selectedFolder
    ? `${selectedFiles.length} files — ${humanSize(total)}`
    : `${selectedFiles[0].name} — ${humanSize(total)}`;
  optionsPanel.style.display = "block";
  document.getElementById("linkBox").style.display = "none";
  document.getElementById("qrCode").style.display = "none";
}

document.getElementById("pickFileBtn").onclick = event => {
  event.stopPropagation();
  fileInput.click();
};
document.getElementById("pickFolderBtn").onclick = event => {
  event.stopPropagation();
  folderInput.click();
};
fileInput.onchange = event => {
  if (event.target.files.length) pickFiles(event.target.files, false);
};
folderInput.onchange = event => {
  if (event.target.files.length) pickFiles(event.target.files, true);
};

["dragenter", "dragover"].forEach(name => {
  dropEl.addEventListener(name, event => {
    event.preventDefault();
    dropEl.classList.add("drag");
  });
});
["dragleave", "drop"].forEach(name => {
  dropEl.addEventListener(name, event => {
    event.preventDefault();
    dropEl.classList.remove("drag");
  });
});
dropEl.addEventListener("drop", event => {
  if (event.dataTransfer.files.length) {
    pickFiles(event.dataTransfer.files, event.dataTransfer.files.length > 1);
  }
});

document.getElementById("cancelBtn").onclick = () => {
  selectedFiles = [];
  selectedFolder = false;
  optionsPanel.style.display = "none";
  fileInput.value = "";
  folderInput.value = "";
};

document.getElementById("createBtn").onclick = async () => {
  if (!selectedFiles.length) return;
  if (!publicReady) {
    toast("Secure public link is not ready yet");
    return;
  }
  const button = document.getElementById("createBtn");
  button.disabled = true;
  button.textContent = "Uploading…";
  const form = new FormData();
  for (const file of selectedFiles) {
    form.append("file", file, file.webkitRelativePath || file.name);
  }
  if (selectedFolder) {
    const firstPath = selectedFiles[0].webkitRelativePath;
    form.append("folder_name", firstPath ? firstPath.split("/")[0] : "shared-folder");
  }
  form.append("expires", document.getElementById("expires").value);
  form.append("max_downloads", document.getElementById("maxDownloads").value);
  form.append("password", document.getElementById("password").value);
  try {
    const endpoint = selectedFolder ? "/api/shares/folder" : "/api/shares";
    const response = await fetch(endpoint, { method: "POST", body: form });
    if (!response.ok) {
      const error = await response.json().catch(() => ({}));
      throw new Error(error.error || "upload failed");
    }
    const result = await response.json();
    const url = result.public_download_url;
    publicBaseUrl = new URL(url).origin;
    document.getElementById("linkOutput").value = url;
    document.getElementById("linkBox").style.display = "flex";
    const qr = document.getElementById("qrCode");
    qr.innerHTML = "";
    new QRCode(qr, { text: url, width: 180, height: 180 });
    qr.style.display = "block";
    toast("Share created");
    loadShares();
  } catch (error) {
    toast("Error: " + error.message);
  } finally {
    button.disabled = false;
    button.textContent = "Create share link";
  }
};

document.getElementById("copyBtn").onclick = () => {
  navigator.clipboard.writeText(document.getElementById("linkOutput").value);
  toast("Link copied");
};

function escapeHtml(value) {
  return value.replace(/[&<>"']/g, character => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;"
  })[character]);
}

async function loadShares() {
  const response = await fetch("/api/shares");
  const shares = await response.json();
  const tbody = document.getElementById("sharesTbody");
  tbody.innerHTML = "";
  for (const share of shares) {
    const row = document.createElement("tr");
    const expires = share.expires_at
      ? new Date(share.expires_at * 1000).toLocaleString()
      : "Never";
    const downloads = share.download_count
      + (share.max_downloads ? " / " + share.max_downloads : "");
    row.innerHTML = `
      <td>${escapeHtml(share.display_name)}${share.is_folder ? " 📁" : ""}${share.has_password ? " 🔒" : ""}</td>
      <td>${humanSize(share.size_bytes)}</td>
      <td>${expires}</td>
      <td>${downloads}</td>
      <td><span class="badge ${share.status}">${share.status}</span></td>
      <td class="actions">
        <button class="secondary" data-copy="${share.id}">Copy link</button>
        <button class="danger" data-revoke="${share.id}">Revoke</button>
      </td>`;
    tbody.appendChild(row);
  }
  tbody.querySelectorAll("[data-copy]").forEach(button => {
    button.onclick = () => {
      navigator.clipboard.writeText(`${publicBaseUrl}/s/${button.dataset.copy}`);
      toast("Link copied");
    };
  });
  tbody.querySelectorAll("[data-revoke]").forEach(button => {
    button.onclick = async () => {
      await fetch(`/api/shares/${button.dataset.revoke}/revoke`, { method: "POST" });
      loadShares();
    };
  });
}

async function loadStatus() {
  try {
    const response = await fetch("/api/status");
    const status = await response.json();
    publicBaseUrl = status.public_base_url;
    publicReady = !status.auto_tunnel || status.tunnel_status === "connected";
    const element = document.getElementById("tunnelStatus");
    if (status.tunnel_status === "connected") {
      element.className = "status connected";
      element.textContent = "Secure public sharing ready: " + publicBaseUrl;
    } else if (!status.auto_tunnel) {
      element.className = "status";
      element.textContent = "Public link: " + publicBaseUrl;
    } else if (["failed", "start_failed", "cloudflared_missing"].includes(status.tunnel_status)) {
      element.className = "status failed";
      element.textContent = "Could not create the secure public link. Restart TempShare and check your internet connection.";
    } else {
      element.className = "status";
      element.textContent = "Preparing secure public link…";
    }
  } catch {
    publicReady = false;
  }
}

document.getElementById("refreshBtn").onclick = loadShares;
loadShares();
loadStatus();
setInterval(loadShares, 10000);
setInterval(loadStatus, 2000);
