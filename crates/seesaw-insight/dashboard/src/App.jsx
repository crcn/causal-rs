import { useState } from 'react'
import Canvas from './components/Canvas'
import Header from './components/Header'
import StreamEntry from './components/StreamEntry'
import { useWebSocket } from './hooks/useWebSocket'
import './App.css'

function App() {
  const [selectedWorkflow, setSelectedWorkflow] = useState(null)
  const { entries, stats, connected } = useWebSocket()

  function handleOrderCreated(data) {
    console.log('Order created successfully:', data)
    // Optionally auto-select the new workflow after a delay
    setTimeout(() => {
      // The correlation_id will match the order_id for new workflows
      // Uncomment to auto-select: setSelectedWorkflow(data.order_id)
    }, 1000)
  }

  return (
    <div className="h-screen flex flex-col overflow-hidden bg-slate-950">
      <Header connected={connected} onCreateOrder={handleOrderCreated} />

      {/* Stats header */}
      <div className="p-6 border-b border-slate-700 bg-slate-900">
        <h3 className="text-lg font-semibold text-slate-50 mb-4">Event Stream</h3>
        <div className="grid grid-cols-4 gap-3 max-w-4xl">
          <div className="bg-slate-950 border border-slate-700 rounded-lg p-3">
            <div className="text-xs text-slate-500 uppercase tracking-wide mb-1">Events</div>
            <div className="text-2xl font-bold text-slate-50">{stats.total_events?.toLocaleString() || '—'}</div>
          </div>
          <div className="bg-slate-950 border border-slate-700 rounded-lg p-3">
            <div className="text-xs text-slate-500 uppercase tracking-wide mb-1">Active</div>
            <div className="text-2xl font-bold text-slate-50">{stats.active_effects?.toLocaleString() || '—'}</div>
          </div>
          <div className="bg-slate-950 border border-slate-700 rounded-lg p-3">
            <div className="text-xs text-slate-500 uppercase tracking-wide mb-1">Stream</div>
            <div className="text-2xl font-bold text-slate-50">{entries.length}</div>
          </div>
          <div className="bg-slate-950 border border-slate-700 rounded-lg p-3">
            <div className="text-xs text-slate-500 uppercase tracking-wide mb-1">Completed</div>
            <div className="text-2xl font-bold text-slate-50">{stats.completed_effects?.toLocaleString() || '—'}</div>
          </div>
        </div>
      </div>

      {/* Accordion list */}
      <div className="flex-1 overflow-y-auto scrollbar-thin">
        <div className="max-w-5xl mx-auto p-6">
          {entries.length === 0 ? (
            <div className="flex items-center justify-center h-full text-center py-20">
              <div>
                <div className="text-5xl mb-4">📡</div>
                <p className="text-slate-500">Waiting for events...</p>
              </div>
            </div>
          ) : (
            entries.map((entry, index) => (
              <StreamEntry
                key={`${entry.correlation_id}-${entry.seq || index}`}
                entry={entry}
                selected={selectedWorkflow === entry.correlation_id}
                onClick={() => setSelectedWorkflow(selectedWorkflow === entry.correlation_id ? null : entry.correlation_id)}
                showTree={selectedWorkflow === entry.correlation_id}
              />
            ))
          )}
        </div>
      </div>
    </div>
  )
}

export default App
