import StreamEntry from './StreamEntry'

export default function Sidebar({ entries, stats, selectedWorkflow, onSelectWorkflow }) {
  if (entries.length === 0) {
    return (
      <div className="w-96 bg-slate-900 border-l border-slate-700 flex flex-col">
        <div className="p-6 border-b border-slate-700">
          <h3 className="text-lg font-semibold text-slate-50 mb-4">Live Stream</h3>
          <div className="grid grid-cols-2 gap-3">
            <div className="bg-slate-950 border border-slate-700 rounded-lg p-3">
              <div className="text-xs text-slate-500 uppercase tracking-wide mb-1">Events</div>
              <div className="text-2xl font-bold text-slate-50">{stats.total_events?.toLocaleString() || '—'}</div>
            </div>
            <div className="bg-slate-950 border border-slate-700 rounded-lg p-3">
              <div className="text-xs text-slate-500 uppercase tracking-wide mb-1">Active</div>
              <div className="text-2xl font-bold text-slate-50">{stats.active_effects?.toLocaleString() || '—'}</div>
            </div>
          </div>
        </div>
        <div className="flex-1 flex items-center justify-center p-8 text-center">
          <div>
            <div className="text-5xl mb-4">📡</div>
            <p className="text-slate-500">Waiting for workflow events...</p>
          </div>
        </div>
      </div>
    )
  }

  return (
    <div className="w-96 bg-slate-900 border-l border-slate-700 flex flex-col">
      <div className="p-6 border-b border-slate-700">
        <h3 className="text-lg font-semibold text-slate-50 mb-4">Live Stream</h3>
        <div className="grid grid-cols-2 gap-3">
          <div className="bg-slate-950 border border-slate-700 rounded-lg p-3">
            <div className="text-xs text-slate-500 uppercase tracking-wide mb-1">Events</div>
            <div className="text-2xl font-bold text-slate-50">{stats.total_events?.toLocaleString() || '—'}</div>
          </div>
          <div className="bg-slate-950 border border-slate-700 rounded-lg p-3">
            <div className="text-xs text-slate-500 uppercase tracking-wide mb-1">Active</div>
            <div className="text-2xl font-bold text-slate-50">{stats.active_effects?.toLocaleString() || '—'}</div>
          </div>
        </div>
      </div>
      <div className="flex-1 overflow-y-auto p-2 scrollbar-thin">
        {entries.map((entry, index) => (
          <StreamEntry
            key={`${entry.correlation_id}-${entry.seq || index}`}
            entry={entry}
            selected={selectedWorkflow === entry.correlation_id}
            onClick={() => onSelectWorkflow(entry.correlation_id)}
          />
        ))}
      </div>
    </div>
  )
}
