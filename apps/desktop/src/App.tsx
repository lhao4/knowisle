import { useCallback, useEffect, useState } from 'react'

import { TreeNode } from './components/FileTree'
import { NoteViewer } from './components/NoteViewer'
import {
  appPing,
  describeError,
  noteRead,
  pickFolder,
  vaultScan,
  type FileNode,
  type NoteContent,
  type ScanResult,
} from './api'

export default function App() {
  const [vault, setVault] = useState<ScanResult | null>(null)
  const [activeNote, setActiveNote] = useState<NoteContent | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [runtime, setRuntime] = useState<string>('')

  // 启动时 ping 一次：确认 Rust 侧真的起来了。
  // 如果这一句失败，后面所有按钮都会失败，报错信息会误导排查方向。
  useEffect(() => {
    appPing()
      .then((info) => setRuntime(`v${info.version} · pid ${info.pid}`))
      .catch((err) => setRuntime(`IPC 未就绪：${describeError(err)}`))
  }, [])

  const pickVault = useCallback(async () => {
    setError(null)
    try {
      const selected = await pickFolder('选择知识库文件夹')
      // 用户点了取消：返回 null，不是错误
      if (selected === null) return

      setBusy(true)
      const result = await vaultScan(selected)
      setVault(result)
      setActiveNote(null)
    } catch (err) {
      setVault(null)
      setActiveNote(null)
      setError(describeError(err))
    } finally {
      setBusy(false)
    }
  }, [])

  const selectNote = useCallback(
    async (node: FileNode) => {
      if (!vault) return
      try {
        const note = await noteRead(vault.root, node.path)
        setActiveNote(note)
        setError(null)
      } catch (err) {
        setError(describeError(err))
      }
    },
    [vault],
  )

  return (
    <div className="app">
      <header className="topbar">
        <div className="brand">
          <span className="brand__mark">◈</span>
          <span className="brand__name">知屿 KnowIsle</span>
          <span className="brand__tag">M0</span>
        </div>

        <div className="topbar__actions">
          {vault && (
            <span className="stats" title={vault.root}>
              {vault.totalFiles} 篇 · {vault.totalDirs} 个目录 · {vault.elapsedMs} ms
              {vault.skipped > 0 && ` · 跳过 ${vault.skipped}`}
            </span>
          )}
          <button type="button" className="btn" onClick={pickVault} disabled={busy}>
            {busy ? '扫描中…' : '选择文件夹'}
          </button>
        </div>
      </header>

      {error && (
        <div className="banner banner--error" role="alert">
          {error}
        </div>
      )}

      {vault?.truncated && (
        <div className="banner banner--warn" role="status">
          文件数超过上限（20000），已截断。当前只索引了前 20000 个条目。
        </div>
      )}

      <main className="workspace">
        <aside className="sidebar">
          {vault ? (
            <>
              <div className="sidebar__root" title={vault.root}>
                {vault.root}
              </div>
              <div className="tree">
                {vault.tree.children?.length ? (
                  vault.tree.children.map((child) => (
                    <TreeNode
                      key={child.path}
                      node={child}
                      depth={0}
                      activePath={activeNote?.path ?? null}
                      onSelect={selectNote}
                    />
                  ))
                ) : (
                  <p className="sidebar__empty">
                    这个文件夹里没有 .md 文件。
                  </p>
                )}
              </div>
            </>
          ) : (
            <div className="sidebar__placeholder">
              <p>还没有打开知识库</p>
              <p className="hint">点右上角「选择文件夹」，选一个装着 .md 的目录。</p>
            </div>
          )}
        </aside>

        <section className="content">
          {activeNote ? (
            <NoteViewer note={activeNote} />
          ) : (
            <div className="welcome">
              <div className="welcome__mark">◈</div>
              <h1>把知识，安放在自己的岛上。</h1>
              <p>
                M0 只做一件事：证明「选文件夹 → 读文件 → 显示内容」这条链路是通的。
              </p>
              <p className="hint">左侧选择一篇笔记试试。{runtime}</p>
            </div>
          )}
        </section>
      </main>
    </div>
  )
}
