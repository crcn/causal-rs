import { useState, useEffect } from 'react'
import TreeView from './TreeView'

function getEntryIcon(entry) {
  if (entry.stream_type === 'event_dispatched') {
    return { symbol: '📤', class: 'bg-blue-500/10 text-blue-400' }
  } else if (entry.stream_type === 'effect_started') {
    return { symbol: '⚙️', class: 'bg-purple-500/10 text-purple-400' }
  } else if (entry.stream_type === 'effect_completed') {
    return { symbol: '✅', class: 'bg-emerald-500/10 text-emerald-400' }
  } else if (entry.stream_type === 'effect_failed') {
    return { symbol: '❌', class: 'bg-red-500/10 text-red-400' }
  }
  return { symbol: '•', class: 'bg-blue-500/10 text-blue-400' }
}

function formatType(type) {
  return type.replace(/_/g, ' ').replace(/\b\w/g, l => l.toUpperCase())
}

function formatDetails(entry) {
  if (entry.stream_type === 'event_dispatched') {
    const eventType = entry.event_type || 'Unknown'
    return `Event: ${eventType}`
  } else if (entry.effect_id) {
    return `Effect: ${entry.effect_id}`
  }
  return 'Workflow activity'
}

export default function StreamEntry({ entry, selected, onClick, showTree }) {
  const [tree, setTree] = useState(null)
  const [loading, setLoading] = useState(false)
  const icon = getEntryIcon(entry)
  const time = new Date(entry.created_at).toLocaleTimeString()

  useEffect(() => {
    if (showTree && !tree && entry.correlation_id) {
      loadTree(entry.correlation_id)
    }
  }, [showTree, entry.correlation_id])

  async function loadTree(correlationId) {
    setLoading(true)
    try {
      const response = await fetch(`/api/tree/${correlationId}`)
      if (response.ok) {
        const data = await response.json()
        setTree(data)
      }
    } catch (err) {
      console.error('Failed to load tree:', err)
    } finally {
      setLoading(false)
    }
  }

  return (
    <div className="mb-3">
      <div
        className={`p-4 rounded-lg border cursor-pointer transition-all ${
          selected
            ? 'bg-slate-900 border-blue-500'
            : 'bg-slate-950 border-slate-700 hover:border-blue-500 hover:bg-slate-900'
        }`}
        onClick={onClick}
      >
        <div className="flex items-center gap-3 mb-2">
          <div className={`w-8 h-8 rounded-lg flex items-center justify-center text-lg flex-shrink-0 ${icon.class}`}>
            {icon.symbol}
          </div>
          <div className="flex-1 min-w-0">
            <div className="text-sm font-semibold text-slate-50">{formatType(entry.stream_type)}</div>
            <div className="text-xs text-slate-500">{time}</div>
          </div>
          {showTree && (
            <div className="text-slate-500">▼</div>
          )}
          {!showTree && selected && (
            <div className="text-slate-500">▶</div>
          )}
        </div>
        <div className="text-sm text-slate-400 mb-2">{formatDetails(entry)}</div>
        <div className="text-xs font-mono text-slate-600 bg-slate-900 px-2 py-1 rounded inline-block">
          {entry.correlation_id.substring(0, 8)}...
        </div>
      </div>

      {/* Accordion content: Tree view */}
      {showTree && (
        <div className="mt-2 ml-4 p-4 bg-slate-900/50 border border-slate-700 rounded-lg">
          {loading ? (
            <div className="text-center py-8 text-slate-500">Loading workflow tree...</div>
          ) : tree ? (
            <TreeView tree={tree} correlationId={entry.correlation_id} />
          ) : (
            <div className="text-center py-8 text-slate-500">No tree data available</div>
          )}
        </div>
      )}
    </div>
  )
}
