import React from 'react';
import { getCapsuleDisplay } from '../lib/display.mjs';
import type { ProviderSnapshot } from '../types/provider';

interface Props {
  snapshot: ProviderSnapshot | null;
  activity?: ProviderSnapshot['activity'];
  expanded?: boolean;
  updateAvailable?: boolean;
  onClick?: () => void;
}

export default function FloatingCapsule({ snapshot, activity, expanded = false, updateAvailable = false, onClick }: Props) {
  const display = getCapsuleDisplay(snapshot);
  const signal = getSignalState(activity ?? snapshot?.activity);

  return (
    <button
      aria-expanded={expanded}
      className="floating-capsule capsule-drag-handle"
      type="button"
      onClick={onClick}
    >
      <div className="traffic-lights" aria-label={signal.label} title={signal.label}>
        <span className={`traffic-light traffic-close ${signal.red ? 'is-active' : ''} ${signal.red && signal.breath ? 'is-breathing' : ''}`} />
        <span className={`traffic-light traffic-minimize ${signal.yellow ? 'is-active' : ''} ${signal.yellow && signal.breath ? 'is-breathing' : ''}`} />
        <span className={`traffic-light traffic-ok ${signal.green ? 'is-active' : ''} ${signal.green && signal.breath ? 'is-breathing' : ''}`} />
      </div>
      <div className="capsule-copy">
        <div className="capsule-heading">
          <strong>{display.title}</strong>
          {display.meta ? <span className="capsule-inline">{display.subtitle}</span> : null}
        </div>
        <span>{display.meta || display.subtitle}</span>
      </div>
      {updateAvailable && (
        <span className="capsule-update-badge" title="发现新版本" aria-label="发现新版本" />
      )}
    </button>
  );
}

function getSignalState(activity: ProviderSnapshot['activity'] | undefined) {
  const status = activity?.status;
  const label = activity?.label || fallbackActivityLabel(status);
  if (status === 'executing' || status === 'answering') {
    return { red: true, yellow: false, green: false, breath: true, label };
  }
  if (status === 'thinking') {
    return { red: false, yellow: true, green: false, breath: true, label };
  }
  if (status === 'waiting_for_user') {
    return { red: true, yellow: false, green: false, breath: true, label };
  }
  if (status === 'auto_reviewing') {
    return { red: false, yellow: true, green: false, breath: true, label };
  }
  if (status === 'finished') {
    return { red: false, yellow: false, green: true, breath: false, label };
  }
  return { red: false, yellow: false, green: true, breath: false, label: label || '状态未知' };
}

function fallbackActivityLabel(status?: string) {
  if (status === 'thinking') return 'Codex 思考中';
  if (status === 'executing') return 'Codex 执行中';
  if (status === 'answering') return 'Codex 执行中';
  if (status === 'waiting_for_user') return 'Codex 等待授权';
  if (status === 'auto_reviewing') return 'Codex 自动审核中';
  if (status === 'finished') return 'Codex 空闲';
  return 'Codex 状态未知';
}
