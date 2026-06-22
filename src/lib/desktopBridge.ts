import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

function isTauriRuntime() {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
}

function installTauriLocalApiFetchBridge() {
  if (!isTauriRuntime()) return;
  const nativeFetch = window.fetch.bind(window);
  window.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = getRequestUrl(input);
    if (!isLocalApiRequest(url)) {
      return nativeFetch(input, init);
    }

    const method = String(init?.method || (input instanceof Request ? input.method : 'GET')).toUpperCase();
    const headers = headersToRecord(init?.headers || (input instanceof Request ? input.headers : undefined));
    const body = typeof init?.body === 'string' ? init.body : undefined;
    const response = await invoke<{
      status?: number;
      headers?: Record<string, string>;
      body?: unknown;
    }>('local_api_request', {
      request: {
        url,
        method,
        headers,
        body
      }
    });

    return new Response(JSON.stringify(response.body ?? {}), {
      status: response.status ?? 200,
      headers: {
        'Content-Type': 'application/json',
        ...(response.headers ?? {})
      }
    });
  };
}

function send(command: string, args?: Record<string, unknown>) {
  invoke(command, args).catch(() => {});
}

function subscribe<T>(event: string, callback: (payload: T) => void) {
  let disposed = false;
  let unlisten: (() => void) | undefined;
  listen<T>(event, (message) => {
    if (!disposed) callback(message.payload);
  })
    .then((dispose) => {
      if (disposed) {
        dispose();
        return;
      }
      unlisten = dispose;
    })
    .catch(() => {});

  return () => {
    disposed = true;
    unlisten?.();
  };
}

function getRequestUrl(input: RequestInfo | URL) {
  if (typeof input === 'string') return input;
  if (input instanceof URL) return input.toString();
  return input.url;
}

function isLocalApiRequest(url: string) {
  return url.startsWith('/local-api/') ||
    url.includes('/local-api/') ||
    url === '/newapi-proxy' ||
    url.includes('/newapi-proxy');
}

function headersToRecord(headers?: HeadersInit) {
  if (!headers) return {};
  if (headers instanceof Headers) {
    return Object.fromEntries(headers.entries());
  }
  if (Array.isArray(headers)) {
    return Object.fromEntries(headers.map(([key, value]) => [key, value]));
  }
  return Object.fromEntries(Object.entries(headers).map(([key, value]) => [key, String(value)]));
}

installTauriLocalApiFetchBridge();

if (isTauriRuntime() && !window.codexQuotaDesktop) {
  window.codexQuotaDesktop = {
    dragStart(point) {
      send('desktop_drag_start', { point });
    },
    dragMove(point) {
      send('desktop_drag_move', { point });
    },
    dragEnd() {
      send('desktop_drag_end');
    },
    setDetailOpen(open) {
      send('desktop_detail_open', { open: Boolean(open) });
    },
    setToastOpen(open) {
      send('desktop_toast_open', { open: Boolean(open) });
    },
    updateLayout(layout) {
      send('desktop_layout_update', { layout });
    },
    updateDetailLayout(layout) {
      send('desktop_detail_layout_update', { layout });
    },
    setSavedPosition(position) {
      send('desktop_saved_position', { position });
    },
    updateHitTestRegions(payload) {
      send('desktop_hit_test_regions', { payload });
    },
    notifyUpdateReady() {
      send('desktop_update_ready');
    },
    dismissUpdateReminder() {
      send('desktop_update_dismiss');
    },
    openUpdateRelease(url) {
      send('desktop_update_open_release', { url });
    },
    openUpdateWindow(options) {
      send('desktop_update_open_window', {
        payload: {
          autoDownload: Boolean(options?.autoDownload)
        }
      });
    },
    startUpdateDownload(asset) {
      send('desktop_update_download', { asset });
    },
    onUpdateDownloadProgress(callback) {
      return subscribe('desktop-update-download-progress', callback);
    },
    onUpdateAutoDownload(callback) {
      return subscribe('desktop-update-auto-download', callback);
    },
    onUpdateDismissed(callback) {
      return subscribe('desktop-update-dismissed', callback);
    },
    onDetailState(callback) {
      return subscribe('desktop-detail-state', callback);
    },
    onPopoverPlacement(callback) {
      return subscribe('desktop-popover-placement', callback);
    },
    onWindowLayout(callback) {
      return subscribe('desktop-window-layout', callback);
    },
    onPositionChanged(callback) {
      return subscribe('desktop-position-changed', callback);
    }
  };
}
