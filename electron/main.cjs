const { app, BrowserWindow, Menu, Tray, ipcMain, nativeImage, net, screen, session, shell } = require('electron');
const fs = require('node:fs');
const fsp = require('node:fs/promises');
const http = require('node:http');
const path = require('node:path');
const { startLocalBackend, stopLocalBackend } = require('./local-backend.cjs');

const ROOT = path.resolve(__dirname, '..');
const PORT = 1420;
const APP_URL = `http://127.0.0.1:${PORT}/`;
const APP_USER_MODEL_ID = 'CodexQuotaGlance.App';
const ICON_PATH = path.join(__dirname, 'icon.ico');
const DEFAULT_CAPSULE_SIZE = { width: 620, height: 72 };
const DEFAULT_DETAIL_SIZE = { width: 520, height: 180 };
const MIN_DETAIL_SIZE = { width: 520, height: 120 };
const WINDOW_MARGIN = 8;
const SCREEN_MARGIN = 8;
const CAPSULE_WINDOW_PAD = 2;
const TOAST_WINDOW_OVERLAP = 1;
const SPEND_TOAST_HEIGHT = 24;
const RESERVED_TOAST_SPACE = SPEND_TOAST_HEIGHT - TOAST_WINDOW_OVERLAP;
const TEXT = {
  showHide: '\u663e\u793a/\u9690\u85cf\u80f6\u56ca',
  settings: '\u8bbe\u7f6e',
  quit: '\u9000\u51fa',
  settingsTitle: 'Codex Quota Glance \u8bbe\u7f6e'
};

let capsuleWindow = null;
let detailWindow = null;
let settingsWindow = null;
let updateWindow = null;
let quotaRecoveryWindow = null;
let quotaRecoveryPromise = null;
let resolveQuotaRecovery = null;
let tray = null;
let ownsBackend = false;
let dragState = null;
let detailOpen = false;
let toastOpen = false;
let capsuleAnchor = null;
let savedCapsulePosition = null;
let detailLayout = { ...DEFAULT_DETAIL_SIZE };
let detailLayoutReady = false;
let detailContentReady = false;
let capsuleUserHidden = false;
let capsuleMouseIgnored = false;
let updateReminderDismissed = false;
let capsuleRecoveryTimer = null;
let capsuleVisibilityGuardTimer = null;
let updateDownloadInProgress = false;
let latestLiveData = null;
let lastWindowLayout = {
  placement: 'bottom',
  offsetX: 0,
  offsetY: 0,
  detailOffset: 0,
  popoverShiftX: 0,
  ready: true
};
let lastLayout = {
  capsule: { x: 0, y: 0, width: DEFAULT_CAPSULE_SIZE.width, height: DEFAULT_CAPSULE_SIZE.height },
  toast: { height: 0 },
  detail: { width: 0, height: 0 }
};

async function createApp() {
  app.setName('Codex Quota Glance');
  app.setAppUserModelId(APP_USER_MODEL_ID);
  Menu.setApplicationMenu(null);
  await ensureBackend();
  createTray();
  await createCapsuleWindow();
  startCapsuleVisibilityGuard();
  setTimeout(() => {
    createUpdateWindow().catch(() => {});
  }, 900);
}

async function createCapsuleWindow() {
  const bounds = screen.getPrimaryDisplay().workArea;
  capsuleWindow = new BrowserWindow({
    width: DEFAULT_CAPSULE_SIZE.width,
    height: DEFAULT_CAPSULE_SIZE.height,
    x: Math.round(bounds.x + bounds.width - DEFAULT_CAPSULE_SIZE.width - 24),
    y: Math.round(bounds.y + 24),
    minWidth: 300,
    minHeight: 56,
    frame: false,
    transparent: true,
    resizable: false,
    maximizable: false,
    minimizable: false,
    skipTaskbar: true,
    show: false,
    autoHideMenuBar: true,
    icon: ICON_PATH,
    backgroundColor: '#00000000',
    alwaysOnTop: true,
    webPreferences: {
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      preload: path.join(__dirname, 'preload.cjs')
    }
  });

  capsuleWindow.setAlwaysOnTop(true, 'screen-saver');
  capsuleWindow.setVisibleOnAllWorkspaces(true, { visibleOnFullScreen: true });
  capsuleWindow.once('ready-to-show', () => {
    if (capsuleUserHidden) return;
    capsuleWindow.showInactive();
    keepFloatingWindowOnTop(capsuleWindow);
    refreshCapsuleMouseHitTest();
  });
  capsuleWindow.on('closed', () => {
    capsuleWindow = null;
    capsuleMouseIgnored = false;
  });
  capsuleWindow.webContents.on('render-process-gone', () => {
    recoverCapsuleWindow();
  });
  capsuleWindow.webContents.on('unresponsive', () => {
    recoverCapsuleWindow();
  });
  capsuleWindow.setMenu(null);
  installWindowHandlers(capsuleWindow);
  await capsuleWindow.loadURL(`${APP_URL}?desktop=1`);
  setCapsuleMousePassthrough(true);
  refreshCapsuleMouseHitTest();
}

async function ensureDetailWindow() {
  if (detailWindow && !detailWindow.isDestroyed()) return detailWindow;

  detailWindow = new BrowserWindow({
    width: DEFAULT_DETAIL_SIZE.width,
    height: DEFAULT_DETAIL_SIZE.height,
    minWidth: DEFAULT_DETAIL_SIZE.width,
    minHeight: MIN_DETAIL_SIZE.height,
    frame: false,
    transparent: true,
    resizable: false,
    maximizable: false,
    minimizable: false,
    skipTaskbar: true,
    show: false,
    opacity: 0,
    autoHideMenuBar: true,
    icon: ICON_PATH,
    backgroundColor: '#00000000',
    alwaysOnTop: true,
    webPreferences: {
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      preload: path.join(__dirname, 'preload.cjs')
    }
  });

  detailWindow.setAlwaysOnTop(true, 'screen-saver');
  detailWindow.setVisibleOnAllWorkspaces(true, { visibleOnFullScreen: true });
  detailWindow.setIgnoreMouseEvents(true);
  detailWindow.webContents.on('did-finish-load', () => {
    detailContentReady = true;
    detailWindow?.webContents.send('desktop-popover-placement', lastWindowLayout.placement);
    sendLiveDataToDetail();
  });
  detailWindow.on('closed', () => {
    detailWindow = null;
    detailOpen = false;
    detailLayoutReady = false;
    detailContentReady = false;
  });
  detailWindow.setMenu(null);
  installWindowHandlers(detailWindow);
  await detailWindow.loadURL(`${APP_URL}?view=detail`);
  detailWindow.showInactive();
  return detailWindow;
}

async function openSettingsWindow() {
  if (settingsWindow && !settingsWindow.isDestroyed()) {
    settingsWindow.show();
    settingsWindow.focus();
    return;
  }

  settingsWindow = new BrowserWindow({
    width: 820,
    height: 620,
    minWidth: 560,
    minHeight: 420,
    show: false,
    autoHideMenuBar: true,
    skipTaskbar: false,
    title: TEXT.settingsTitle,
    backgroundColor: '#f6f8fb',
    icon: ICON_PATH,
    webPreferences: {
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      preload: path.join(__dirname, 'preload.cjs')
    }
  });

  const showSettings = () => {
    if (!settingsWindow || settingsWindow.isDestroyed()) return;
    settingsWindow.show();
    settingsWindow.focus();
  };
  settingsWindow.once('ready-to-show', showSettings);
  settingsWindow.webContents.once('did-finish-load', showSettings);
  setTimeout(showSettings, 500);
  settingsWindow.on('closed', () => {
    settingsWindow = null;
  });
  settingsWindow.setMenu(null);
  installWindowHandlers(settingsWindow);
  await settingsWindow.loadURL(`${APP_URL}?view=settings`);
}

async function createUpdateWindow(options = {}) {
  const force = options.force === true;
  const autoDownload = options.autoDownload === true;
  if (updateReminderDismissed && !force) return;
  if (updateWindow && !updateWindow.isDestroyed()) {
    if (autoDownload) updateWindow.webContents.send('desktop-update-auto-download');
    return;
  }

  updateWindow = new BrowserWindow({
    width: 460,
    height: 320,
    minWidth: 420,
    minHeight: 280,
    show: false,
    resizable: false,
    maximizable: false,
    minimizable: false,
    autoHideMenuBar: true,
    skipTaskbar: false,
    title: 'Codex Quota Glance 更新',
    backgroundColor: '#f6f8fb',
    icon: ICON_PATH,
    webPreferences: {
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      preload: path.join(__dirname, 'preload.cjs')
    }
  });

  updateWindow.on('closed', () => {
    updateWindow = null;
  });
  updateWindow.setMenu(null);
  installWindowHandlers(updateWindow);
  const updateUrl = `${APP_URL}?${autoDownload ? 'view=update&download=1' : 'view=update'}`;
  await updateWindow.loadURL(updateUrl);
}

async function showQuotaRecoveryWindow(payload = {}) {
  if (quotaRecoveryPromise) return quotaRecoveryPromise;
  if (!capsuleWindow || capsuleWindow.isDestroyed()) return 'dismissed';

  quotaRecoveryPromise = new Promise((resolve) => {
    resolveQuotaRecovery = resolve;
  });
  const resultPromise = quotaRecoveryPromise;
  const width = 420;
  const height = 220;
  const display = screen.getDisplayMatching(capsuleWindow.getBounds());
  const x = Math.round(display.workArea.x + (display.workArea.width - width) / 2);
  const y = Math.round(display.workArea.y + (display.workArea.height - height) / 2);

  quotaRecoveryWindow = new BrowserWindow({
    width,
    height,
    x,
    y,
    parent: capsuleWindow,
    modal: true,
    frame: true,
    show: false,
    resizable: false,
    maximizable: false,
    minimizable: false,
    autoHideMenuBar: true,
    skipTaskbar: false,
    title: '官方额度已恢复',
    backgroundColor: '#f6f8fb',
    icon: ICON_PATH,
    webPreferences: {
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      preload: path.join(__dirname, 'preload.cjs')
    }
  });

  quotaRecoveryWindow.once('ready-to-show', () => {
    quotaRecoveryWindow?.show();
    quotaRecoveryWindow?.focus();
  });
  quotaRecoveryWindow.on('closed', () => {
    quotaRecoveryWindow = null;
    settleQuotaRecovery('dismissed');
  });
  quotaRecoveryWindow.setMenu(null);
  installWindowHandlers(quotaRecoveryWindow);
  await quotaRecoveryWindow.loadURL(`${APP_URL}?view=quota-recovery&resetAt=${encodeURIComponent(String(payload.resetAt || ''))}`);
  return resultPromise;
}

function settleQuotaRecovery(result) {
  const resolve = resolveQuotaRecovery;
  resolveQuotaRecovery = null;
  quotaRecoveryPromise = null;
  resolve?.(result);
}

function createTray() {
  tray = new Tray(createTrayImage());
  tray.setToolTip('Codex Quota Glance');
  tray.setContextMenu(buildTrayMenu());
  tray.on('click', () => toggleCapsuleWindow());
}

function buildTrayMenu() {
  return Menu.buildFromTemplate([
    { label: TEXT.showHide, click: () => toggleCapsuleWindow() },
    { label: TEXT.settings, click: () => openSettingsWindow().catch(() => {}) },
    { type: 'separator' },
    { label: TEXT.quit, click: () => app.quit() }
  ]);
}

function toggleCapsuleWindow() {
  if (!capsuleWindow || capsuleWindow.isDestroyed()) {
    createCapsuleWindow().catch(() => {});
    return;
  }
  if (capsuleWindow.isVisible()) {
    capsuleUserHidden = true;
    detailOpen = false;
    hideDetailWindow();
    capsuleWindow.hide();
  } else {
    capsuleUserHidden = false;
    restoreCapsuleAfterDisplayChange();
  }
}

function createTrayImage() {
  const image = nativeImage.createFromPath(ICON_PATH);
  if (!image.isEmpty()) {
    return image.resize({ width: 16, height: 16 });
  }
  const svg = '<svg width="32" height="32" xmlns="http://www.w3.org/2000/svg"><rect width="32" height="32" rx="8" fill="#17212b"/><circle cx="10.5" cy="15.5" r="4" fill="#ffbd2e"/><circle cx="21.5" cy="15.5" r="4" fill="#28c840"/></svg>';
  return nativeImage.createFromDataURL(`data:image/svg+xml;base64,${Buffer.from(svg).toString('base64')}`);
}

function installWindowHandlers(win) {
  win.webContents.setWindowOpenHandler(({ url }) => {
    shell.openExternal(url);
    return { action: 'deny' };
  });
  win.webContents.on('context-menu', () => {
    Menu.buildFromTemplate([
      { label: TEXT.showHide, click: () => toggleCapsuleWindow() },
      { label: TEXT.settings, click: () => openSettingsWindow().catch(() => {}) },
      { type: 'separator' },
      { label: TEXT.quit, click: () => app.quit() }
    ]).popup({ window: win });
  });
}

ipcMain.on('desktop-drag-start', (event, point) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (!win || win !== capsuleWindow) return;
  setCapsuleMousePassthrough(false);
  const capsule = getCurrentCapsuleScreenBounds();
  dragState = {
    mouseX: Number(point?.screenX) || 0,
    mouseY: Number(point?.screenY) || 0,
    bounds: win.getBounds(),
    capsule
  };
});

ipcMain.on('desktop-drag-move', (event, point) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (!win || win !== capsuleWindow || !dragState) return;
  const nextX = dragState.bounds.x + Math.round((Number(point?.screenX) || 0) - dragState.mouseX);
  const nextY = dragState.bounds.y + Math.round((Number(point?.screenY) || 0) - dragState.mouseY);
  win.setPosition(nextX, nextY, false);
});

ipcMain.on('desktop-drag-end', (event) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (win && win === capsuleWindow) {
    const capsule = getCurrentCapsuleScreenBounds();
    savedCapsulePosition = { x: capsule.x, y: capsule.y };
    capsuleAnchor = capsule;
    sendPositionChanged(savedCapsulePosition);
    keepFloatingWindowOnTop();
    if (detailOpen) positionDetailWindow();
  }
  dragState = null;
});

ipcMain.on('desktop-hit-test-regions', (event, payload) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (!win || win !== capsuleWindow) return;
  if (dragState) {
    setCapsuleMousePassthrough(false);
    return;
  }
  setCapsuleMousePassthrough(!Boolean(payload?.interactive));
});

ipcMain.on('desktop-detail-open', (event, open) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (!win || win !== capsuleWindow) return;
  detailOpen = Boolean(open);
  if (detailOpen) {
    showDetailWindow().catch(() => {});
    return;
  }
  hideDetailWindow();
});

ipcMain.on('desktop-toast-open', (event, open) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (!win || win !== capsuleWindow) return;
  toastOpen = Boolean(open);
  resizeCapsuleWindow();
});

ipcMain.handle('desktop-quota-recovery-open', (event, payload) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (!win || win !== capsuleWindow) return 'dismissed';
  return showQuotaRecoveryWindow(payload);
});

ipcMain.on('desktop-quota-recovery-confirm', (event) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (!win || win !== quotaRecoveryWindow) return;
  settleQuotaRecovery('confirmed');
  quotaRecoveryWindow.close();
});

ipcMain.on('desktop-layout-update', (event, layout) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (!win || win !== capsuleWindow) return;
  lastLayout = normalizeLayout(layout);
  resizeCapsuleWindow();
  if (detailOpen) positionDetailWindow();
});

ipcMain.on('desktop-detail-layout-update', (event, layout) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (!win || win !== detailWindow) return;
  detailLayout = normalizeSize(layout, DEFAULT_DETAIL_SIZE);
  detailLayoutReady = true;
  if (detailOpen) positionDetailWindow();
});

ipcMain.on('desktop-saved-position', (event, position) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (!win || win !== capsuleWindow) return;
  const next = normalizePosition(position);
  if (!next) return;
  savedCapsulePosition = next;
  if (!dragState) {
    capsuleAnchor = {
      x: next.x,
      y: next.y,
      width: lastLayout.capsule.width,
      height: lastLayout.capsule.height
    };
    resizeCapsuleWindow();
    if (detailOpen) positionDetailWindow();
  }
});

ipcMain.on('desktop-live-data-publish', (event, payload) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (!win || win !== capsuleWindow || !payload || typeof payload !== 'object') return;
  latestLiveData = payload;
  sendLiveDataToDetail();
});

ipcMain.on('desktop-live-data-request', (event) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (!win || win !== detailWindow) return;
  sendLiveDataToDetail();
});

ipcMain.on('desktop-update-ready', (event) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (!win || win !== updateWindow || updateReminderDismissed) return;
  updateWindow.show();
  updateWindow.focus();
});

ipcMain.on('desktop-update-open-window', (_event, payload) => {
  createUpdateWindow({ force: true, autoDownload: Boolean(payload?.autoDownload) })
    .then(() => {
      if (updateWindow && !updateWindow.isDestroyed()) {
        updateWindow.show();
        updateWindow.focus();
      }
    })
    .catch(() => {});
});

ipcMain.on('desktop-update-dismiss', (event) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (win && updateWindow && win !== updateWindow) return;
  updateReminderDismissed = true;
  BrowserWindow.getAllWindows().forEach((window) => {
    window.webContents.send('desktop-update-dismissed');
  });
  if (updateWindow && !updateWindow.isDestroyed()) {
    updateWindow.close();
  }
});

ipcMain.on('desktop-update-open-release', (event, url) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (win && updateWindow && win !== updateWindow) return;
  if (typeof url === 'string' && /^https:\/\/github\.com\/akitten-cn\/codex-quota-glance\/releases/.test(url)) {
    shell.openExternal(url);
  }
});

ipcMain.on('desktop-update-download', (event, asset) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (win && updateWindow && win !== updateWindow) return;
  downloadUpdateInstaller(asset, win || updateWindow).catch((error) => {
    sendUpdateDownloadProgress(win || updateWindow, {
      status: 'error',
      message: error instanceof Error ? error.message : String(error || '下载失败')
    });
  });
});

function resizeCapsuleWindow() {
  if (!capsuleWindow || capsuleWindow.isDestroyed()) return;
  if (dragState) return;
  const current = capsuleWindow.getBounds();
  const layout = lastLayout;
  const capsuleSize = normalizeSize(layout.capsule, DEFAULT_CAPSULE_SIZE);
  const currentCapsuleScreen = getCapsuleAnchorCandidate(current, capsuleSize);
  const workArea = getDisplayForCapsuleBounds(currentCapsuleScreen, current).workArea;
  const anchoredCapsuleScreen = clampCapsuleToWorkArea(currentCapsuleScreen, capsuleSize, workArea);

  const width = capsuleSize.width + CAPSULE_WINDOW_PAD;
  const toastSpace = RESERVED_TOAST_SPACE;
  const height = capsuleSize.height + toastSpace + CAPSULE_WINDOW_PAD;
  const x = clamp(anchoredCapsuleScreen.x, workArea.x + SCREEN_MARGIN, workArea.x + workArea.width - width - SCREEN_MARGIN);
  const y = clamp(
    anchoredCapsuleScreen.y - toastSpace,
    workArea.y + SCREEN_MARGIN,
    workArea.y + workArea.height - height - SCREEN_MARGIN
  );
  const offsetX = clamp(anchoredCapsuleScreen.x - x, 0, Math.max(0, width - capsuleSize.width));
  const offsetY = clamp(anchoredCapsuleScreen.y - y, 0, Math.max(0, height - capsuleSize.height));
  capsuleAnchor = { x: x + offsetX, y: y + offsetY, width: capsuleSize.width, height: capsuleSize.height };
  const changed = setCapsuleWindowBounds({ x, y, width, height });
  sendWindowLayout({ placement: 'bottom', offsetX, offsetY, detailOffset: 0, popoverShiftX: 0 });
  if (changed) keepFloatingWindowOnTop();
  refreshCapsuleMouseHitTest();
}

async function showDetailWindow() {
  const win = await ensureDetailWindow();
  if (!detailOpen || !capsuleWindow || capsuleWindow.isDestroyed()) return;
  if (!detailContentReady || !detailLayoutReady) {
    await waitForDetailReadiness();
    if (!detailOpen || win.isDestroyed()) return;
  }
  positionDetailWindow();
  await new Promise((resolve) => setTimeout(resolve, 32));
  if (!detailOpen || win.isDestroyed()) return;
  win.setIgnoreMouseEvents(false);
  win.setOpacity(1);
  if (!win.isVisible()) win.showInactive();
  keepFloatingWindowOnTop(win);
}

function waitForDetailReadiness() {
  const startedAt = Date.now();
  return new Promise((resolve) => {
    const tick = () => {
      if ((detailContentReady && detailLayoutReady) || Date.now() - startedAt > 360) {
        resolve();
        return;
      }
      setTimeout(tick, 16);
    };
    tick();
  });
}

function hideDetailWindow() {
  if (!detailWindow || detailWindow.isDestroyed()) return;
  detailWindow.setIgnoreMouseEvents(true);
  detailWindow.setOpacity(0);
}

function positionDetailWindow() {
  if (!detailWindow || detailWindow.isDestroyed() || !capsuleWindow || capsuleWindow.isDestroyed()) return;
  const capsule = getCurrentCapsuleScreenBounds();
  const detailSize = normalizeSize(detailLayout, DEFAULT_DETAIL_SIZE);
  detailSize.width = Math.max(MIN_DETAIL_SIZE.width, detailSize.width);
  detailSize.height = Math.max(MIN_DETAIL_SIZE.height, detailSize.height);
  const display = screen.getDisplayMatching({
    x: capsule.x,
    y: capsule.y,
    width: capsule.width,
    height: capsule.height
  });
  const workArea = display.workArea;
  const below = workArea.y + workArea.height - (capsule.y + capsule.height);
  const above = capsule.y - workArea.y;
  const placement = below < detailSize.height + WINDOW_MARGIN && above > below ? 'top' : 'bottom';
  const x = clamp(
    Math.round(capsule.x + (capsule.width - detailSize.width) / 2),
    workArea.x + SCREEN_MARGIN,
    workArea.x + workArea.width - detailSize.width - SCREEN_MARGIN
  );
  const desiredY = placement === 'top'
    ? capsule.y - detailSize.height - WINDOW_MARGIN
    : capsule.y + capsule.height + WINDOW_MARGIN;
  const y = clamp(
    Math.round(desiredY),
    workArea.y + SCREEN_MARGIN,
    workArea.y + workArea.height - detailSize.height - SCREEN_MARGIN
  );
  detailWindow.setBounds({ x, y, width: detailSize.width, height: detailSize.height }, false);
  detailWindow.webContents.send('desktop-popover-placement', placement);
}

function getAnchoredCapsuleBounds(current, capsuleSize, workArea) {
  return clampCapsuleToWorkArea(getCapsuleAnchorCandidate(current, capsuleSize), capsuleSize, workArea);
}

function getCapsuleAnchorCandidate(current, capsuleSize) {
  const base = capsuleAnchor || (
    savedCapsulePosition
      ? { x: savedCapsulePosition.x, y: savedCapsulePosition.y, width: capsuleSize.width, height: capsuleSize.height }
      : {
          x: current.x + finite(lastWindowLayout.offsetX, 0),
          y: current.y + finite(lastWindowLayout.offsetY, 0),
          width: capsuleSize.width,
          height: capsuleSize.height
        }
  );
  return { x: base.x, y: base.y, width: capsuleSize.width, height: capsuleSize.height };
}

function clampCapsuleToWorkArea(base, capsuleSize, workArea) {
  const x = clamp(base.x, workArea.x + SCREEN_MARGIN, workArea.x + workArea.width - capsuleSize.width - SCREEN_MARGIN);
  const y = clamp(base.y, workArea.y + SCREEN_MARGIN, workArea.y + workArea.height - capsuleSize.height - SCREEN_MARGIN);
  return { x, y, width: capsuleSize.width, height: capsuleSize.height };
}

function getDisplayForCapsuleBounds(capsuleBounds, fallbackBounds) {
  const candidate = {
    x: Math.round(finite(capsuleBounds?.x, fallbackBounds?.x ?? 0)),
    y: Math.round(finite(capsuleBounds?.y, fallbackBounds?.y ?? 0)),
    width: Math.max(1, Math.round(finite(capsuleBounds?.width, fallbackBounds?.width ?? DEFAULT_CAPSULE_SIZE.width))),
    height: Math.max(1, Math.round(finite(capsuleBounds?.height, fallbackBounds?.height ?? DEFAULT_CAPSULE_SIZE.height)))
  };
  return screen.getDisplayMatching(candidate);
}

function getCurrentCapsuleScreenBounds() {
  const bounds = capsuleWindow?.getBounds() || { x: 0, y: 0 };
  const capsule = normalizeSize(lastLayout.capsule, DEFAULT_CAPSULE_SIZE);
  const offsetX = finite(lastWindowLayout.offsetX, 0);
  const offsetY = finite(lastWindowLayout.offsetY, 0);
  return {
    x: bounds.x + offsetX,
    y: bounds.y + offsetY,
    width: capsule.width,
    height: capsule.height
  };
}

function sendWindowLayout(layout) {
  if (!capsuleWindow || capsuleWindow.isDestroyed()) return;
  lastWindowLayout = {
    placement: layout.placement === 'top' ? 'top' : 'bottom',
    offsetX: Math.round(finite(layout.offsetX, 0)),
    offsetY: Math.round(finite(layout.offsetY, 0)),
    detailOffset: Math.round(finite(layout.detailOffset, 0)),
    popoverShiftX: Math.round(finite(layout.popoverShiftX, 0)),
    ready: layout.ready !== false
  };
  capsuleWindow.webContents.send('desktop-popover-placement', lastWindowLayout.placement);
  capsuleWindow.webContents.send('desktop-window-layout', lastWindowLayout);
}

function sendPositionChanged(position) {
  if (!capsuleWindow || capsuleWindow.isDestroyed() || !position) return;
  capsuleWindow.webContents.send('desktop-position-changed', {
    x: Math.round(finite(position.x, 0)),
    y: Math.round(finite(position.y, 0))
  });
}

function keepFloatingWindowOnTop(win = capsuleWindow) {
  if (!win || win.isDestroyed()) return;
  win.setAlwaysOnTop(true, 'screen-saver');
  win.moveTop();
}

function startCapsuleVisibilityGuard() {
  if (capsuleVisibilityGuardTimer) return;
  capsuleVisibilityGuardTimer = setInterval(restoreCapsuleAfterDisplayChange, 2000);
  screen.on('display-added', restoreCapsuleAfterDisplayChange);
  screen.on('display-removed', restoreCapsuleAfterDisplayChange);
  screen.on('display-metrics-changed', restoreCapsuleAfterDisplayChange);
  app.on('browser-window-focus', restoreCapsuleAfterDisplayChange);
}

function stopCapsuleVisibilityGuard() {
  if (capsuleVisibilityGuardTimer) {
    clearInterval(capsuleVisibilityGuardTimer);
    capsuleVisibilityGuardTimer = null;
  }
  screen.off('display-added', restoreCapsuleAfterDisplayChange);
  screen.off('display-removed', restoreCapsuleAfterDisplayChange);
  screen.off('display-metrics-changed', restoreCapsuleAfterDisplayChange);
  app.off('browser-window-focus', restoreCapsuleAfterDisplayChange);
}

function restoreCapsuleAfterDisplayChange() {
  if (capsuleUserHidden) return;
  if (!capsuleWindow || capsuleWindow.isDestroyed()) {
    createCapsuleWindow().catch(() => {});
    return;
  }
  const wasVisible = capsuleWindow.isVisible();
  resizeCapsuleWindow();
  if (!wasVisible) {
    capsuleWindow.showInactive();
  }
  if (!wasVisible || !capsuleWindow.isAlwaysOnTop()) {
    keepFloatingWindowOnTop(capsuleWindow);
  }
  refreshCapsuleMouseHitTest();
  if (detailOpen) positionDetailWindow();
}

function refreshCapsuleMouseHitTest() {
  if (!capsuleWindow || capsuleWindow.isDestroyed()) return;
  if (dragState) {
    setCapsuleMousePassthrough(false);
    return;
  }
  if (!capsuleWindow.isVisible()) {
    setCapsuleMousePassthrough(true);
    return;
  }
  const point = screen.getCursorScreenPoint();
  const interactive = getCapsuleInteractiveBounds().some((bounds) => pointInRect(point, bounds));
  setCapsuleMousePassthrough(!interactive);
}

function getCapsuleInteractiveBounds() {
  const capsule = getCurrentCapsuleScreenBounds();
  if (!toastOpen) return [capsule];
  return [
    capsule,
    {
      x: capsule.x,
      y: capsule.y - RESERVED_TOAST_SPACE,
      width: capsule.width,
      height: capsule.height + RESERVED_TOAST_SPACE
    }
  ];
}

function pointInRect(point, rect) {
  return point.x >= rect.x &&
    point.x <= rect.x + rect.width &&
    point.y >= rect.y &&
    point.y <= rect.y + rect.height;
}

function setCapsuleMousePassthrough(ignore) {
  if (!capsuleWindow || capsuleWindow.isDestroyed()) return;
  const next = Boolean(ignore);
  if (capsuleMouseIgnored === next) return;
  capsuleMouseIgnored = next;
  if (next) {
    capsuleWindow.setIgnoreMouseEvents(true, { forward: true });
  } else {
    capsuleWindow.setIgnoreMouseEvents(false);
  }
}

function recoverCapsuleWindow() {
  if (capsuleRecoveryTimer || capsuleUserHidden) return;
  capsuleRecoveryTimer = setTimeout(() => {
    capsuleRecoveryTimer = null;
    if (capsuleUserHidden) return;
    if (capsuleWindow && !capsuleWindow.isDestroyed()) {
      capsuleWindow.reload();
      return;
    }
    createCapsuleWindow().catch(() => {});
  }, 250);
}

async function downloadUpdateInstaller(asset, win) {
  if (updateDownloadInProgress) return;
  const name = String(asset?.name || '');
  const url = String(asset?.url || '');
  if (!isTrustedUpdateAsset(name, url)) {
    throw new Error('没有找到可信的 Windows 安装包');
  }
  updateDownloadInProgress = true;
  try {
    const downloadsDir = path.join(app.getPath('temp'), 'CodexQuotaGlance', 'updates');
    await fsp.mkdir(downloadsDir, { recursive: true });
    const target = path.join(downloadsDir, safeFileName(name));
    const result = await downloadWithElectronNet(url, target, Number(asset?.size) || 0, (progress) => {
      sendUpdateDownloadProgress(win, { status: 'downloading', message: '正在下载安装包...', ...progress });
    });
    const received = result.received;
    const total = result.total;
    sendUpdateDownloadProgress(win, { status: 'launching', received, total, percent: 100, message: '下载完成，正在关闭当前程序并启动安装程序...' });
    app.relaunch({ execPath: target, args: [] });
    app.quit();
  } finally {
    updateDownloadInProgress = false;
  }
}

async function downloadWithElectronNet(url, target, expectedSize, onProgress) {
  await session.defaultSession.resolveProxy(url);
  return new Promise((resolve, reject) => {
    const request = net.request({
      method: 'GET',
      url,
      useSessionCookies: false
    });
    request.setHeader('User-Agent', 'CodexQuotaGlance/0.1');
    request.on('response', (response) => {
      const statusCode = Number(response.statusCode) || 0;
      if (statusCode < 200 || statusCode >= 300) {
        reject(new Error(`下载安装包失败：HTTP ${statusCode || 'unknown'}`));
        response.resume();
        return;
      }
      const total = Number(response.headers['content-length']) || expectedSize || 0;
      const stream = fs.createWriteStream(target);
      let received = 0;
      onProgress?.({ received, total, percent: 0 });
      response.on('data', (chunk) => {
        received += chunk.length;
        stream.write(chunk);
        const percent = total > 0 ? Math.min(100, Math.round((received / total) * 100)) : 0;
        onProgress?.({ received, total, percent });
      });
      response.on('end', () => {
        stream.end((error) => {
          if (error) {
            reject(error);
            return;
          }
          resolve({ received, total });
        });
      });
      response.on('error', reject);
      stream.on('error', reject);
    });
    request.on('error', reject);
    request.end();
  });
}

function sendUpdateDownloadProgress(win, payload) {
  const targets = new Set([win, updateWindow].filter((window) => window && !window.isDestroyed()));
  for (const target of targets) {
    target.webContents.send('desktop-update-download-progress', payload);
  }
}

function isTrustedUpdateAsset(name, url) {
  const lowerName = String(name || '').toLowerCase();
  return lowerName.endsWith('.exe') &&
    lowerName.includes('win') &&
    !lowerName.includes('portable') &&
    /^https:\/\/github\.com\/akitten-cn\/codex-quota-glance\/releases\/download\//.test(String(url || ''));
}

function safeFileName(name) {
  return String(name || 'CodexQuotaGlance-update.exe').replace(/[<>:"/\\|?*\x00-\x1F]/g, '_');
}

function setCapsuleWindowBounds(bounds) {
  if (!capsuleWindow || capsuleWindow.isDestroyed()) return false;
  const current = capsuleWindow.getBounds();
  const next = {
    x: Math.round(finite(bounds.x, current.x)),
    y: Math.round(finite(bounds.y, current.y)),
    width: Math.round(finite(bounds.width, current.width)),
    height: Math.round(finite(bounds.height, current.height))
  };
  if (
    current.x === next.x &&
    current.y === next.y &&
    current.width === next.width &&
    current.height === next.height
  ) {
    return false;
  }
  capsuleWindow.setBounds(next, false);
  return true;
}

function normalizeLayout(layout) {
  const capsule = normalizeRect(layout?.capsule, DEFAULT_CAPSULE_SIZE);
  const toast = { height: Math.max(0, finite(layout?.toast?.height, 0)) };
  return { capsule, toast, detail: { width: 0, height: 0 } };
}

function normalizeRect(value, fallback) {
  return {
    x: finite(value?.x, 0),
    y: finite(value?.y, 0),
    width: Math.max(1, finite(value?.width, fallback.width)),
    height: Math.max(1, finite(value?.height, fallback.height))
  };
}

function normalizeSize(value, fallback) {
  return {
    width: Math.max(1, Math.ceil(finite(value?.width, fallback.width))),
    height: Math.max(1, Math.ceil(finite(value?.height, fallback.height)))
  };
}

function normalizePosition(value) {
  const x = Number(value?.x);
  const y = Number(value?.y);
  if (!Number.isFinite(x) || !Number.isFinite(y)) return null;
  return { x: Math.round(x), y: Math.round(y) };
}

function finite(value, fallback) {
  const number = Number(value);
  return Number.isFinite(number) ? number : fallback;
}

function clamp(value, min, max) {
  return Math.max(min, Math.min(value, max));
}

function notifyCapsuleDataInvalidated() {
  if (!capsuleWindow || capsuleWindow.isDestroyed() || capsuleWindow.webContents.isDestroyed()) return;
  capsuleWindow.webContents.send('desktop-data-invalidated');
}

function sendLiveDataToDetail() {
  if (!latestLiveData || !detailWindow || detailWindow.isDestroyed() || detailWindow.webContents.isDestroyed()) return;
  detailWindow.webContents.send('desktop-live-data', latestLiveData);
}

async function ensureBackend() {
  if (await canConnect()) return;
  const appRoot = app.getAppPath();
  await startLocalBackend({
    appRoot,
    distDir: path.join(appRoot, 'dist'),
    onCodexSessionChanged: notifyCapsuleDataInvalidated
  });
  ownsBackend = true;

  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    if (await canConnect()) return;
    await delay(250);
  }
  throw new Error(`Local backend failed to start on ${APP_URL}`);
}

function canConnect() {
  return new Promise((resolve) => {
    const req = http.get(`${APP_URL}local-api/health`, (res) => {
      let body = '';
      res.setEncoding('utf8');
      res.on('data', (chunk) => {
        body += chunk;
      });
      res.on('end', () => {
        try {
          const payload = JSON.parse(body);
          resolve(res.statusCode === 200 && payload?.backend === 'electron-local-backend');
        } catch {
          resolve(false);
        }
      });
    });
    req.on('error', () => resolve(false));
    req.setTimeout(800, () => {
      req.destroy();
      resolve(false);
    });
  });
}

function delay(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

app.whenReady().then(createApp);

app.on('window-all-closed', (event) => {
  event.preventDefault();
});

app.on('before-quit', async () => {
  if (capsuleRecoveryTimer) {
    clearTimeout(capsuleRecoveryTimer);
    capsuleRecoveryTimer = null;
  }
  stopCapsuleVisibilityGuard();
  if (ownsBackend) {
    await stopLocalBackend();
    ownsBackend = false;
  }
});
