import { useState, useEffect, useRef } from 'react'

export function useWebSocket() {
  const [entries, setEntries] = useState([])
  const [stats, setStats] = useState({ total_events: 0, active_effects: 0 })
  const [connected, setConnected] = useState(false)
  const ws = useRef(null)
  const reconnectTimeout = useRef(null)

  useEffect(() => {
    connectWebSocket()
    fetchStats()
    const statsInterval = setInterval(fetchStats, 5000)

    return () => {
      clearInterval(statsInterval)
      if (reconnectTimeout.current) clearTimeout(reconnectTimeout.current)
      if (ws.current) ws.current.close()
    }
  }, [])

  function connectWebSocket() {
    const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    const url = `${protocol}//${window.location.host}/api/ws`

    ws.current = new WebSocket(url)

    ws.current.onopen = () => {
      setConnected(true)
      console.log('WebSocket connected')
    }

    ws.current.onmessage = (event) => {
      const data = JSON.parse(event.data)
      if (data.type !== 'connected') {
        setEntries(prev => {
          // Deduplicate by seq - don't add if we already have this seq
          if (prev.some(e => e.seq === data.seq)) {
            return prev
          }
          const newEntries = [data, ...prev]
          return newEntries.slice(0, 100) // Keep last 100 entries
        })
      }
    }

    ws.current.onerror = (error) => {
      console.error('WebSocket error:', error)
    }

    ws.current.onclose = () => {
      setConnected(false)
      console.log('WebSocket closed, reconnecting...')
      reconnectTimeout.current = setTimeout(connectWebSocket, 1000)
    }
  }

  async function fetchStats() {
    try {
      const response = await fetch('/api/stats')
      const data = await response.json()
      setStats(data)
    } catch (error) {
      console.error('Failed to fetch stats:', error)
    }
  }

  return { entries, stats, connected }
}
