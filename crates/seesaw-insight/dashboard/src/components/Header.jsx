import { useState } from 'react'

export default function Header({ connected, onCreateOrder }) {
  const [creating, setCreating] = useState(false)

  async function handleCreateOrder() {
    setCreating(true)
    try {
      const response = await fetch('http://localhost:3001/create-order', {
        method: 'POST',
      })
      if (response.ok) {
        const data = await response.json()
        console.log('Order created:', data)
        if (onCreateOrder) onCreateOrder(data)
      }
    } catch (error) {
      console.error('Failed to create order:', error)
    } finally {
      setCreating(false)
    }
  }

  return (
    <header className="bg-slate-900 border-b border-slate-700 px-8 py-4 flex justify-between items-center">
      <div>
        <h1 className="text-2xl font-bold text-slate-50">Seesaw Insight</h1>
        <p className="text-sm text-slate-400">Real-time workflow monitoring</p>
      </div>
      <div className="flex items-center gap-4">
        <button
          onClick={handleCreateOrder}
          disabled={creating}
          className="px-4 py-2 bg-blue-600 hover:bg-blue-700 disabled:bg-slate-700 disabled:cursor-not-allowed text-white rounded-lg text-sm font-medium transition-colors flex items-center gap-2"
        >
          {creating ? (
            <>
              <span className="w-4 h-4 border-2 border-white border-t-transparent rounded-full animate-spin" />
              Creating...
            </>
          ) : (
            <>
              <span>📦</span>
              Create Order
            </>
          )}
        </button>
        <div className="flex items-center gap-3 px-4 py-2 bg-emerald-500/10 text-emerald-400 rounded-full text-sm font-medium">
          <span className={`w-2 h-2 rounded-full ${connected ? 'bg-emerald-400 animate-pulse' : 'bg-slate-500'}`} />
          <span>{connected ? 'Connected' : 'Disconnected'}</span>
        </div>
      </div>
    </header>
  )
}
