import { useCallback, useEffect, useRef, useState } from 'react'

import { TreeNode } from './components/FileTree'
import { NoteEditor, type NoteEditorHandle } from './components/NoteEditor'
import {
  appPing,
  describeError,
  noteEdit,
  noteFlush,
  noteRead,
  noteStatus,
  pickFolder,
  vaultScan,
  type FileNode,
  type NoteContent,
  type SaveState,
  type ScanResult,
} from './api'

/**
 * 前端多久查一次保存状态。
 *
 * 只在 `dirty` 为真时轮询——自动落盘发生在 Rust 侧，前端没有别的办法
 * 知道它什么时候完成。空闲时完全不轮询，所以稳态下开销是零。
 */
const STATUS_POLL_MS = 500

/** 上次打开的 Vault 绝对路径。 */
const LAST_VAULT_KEY = 'knowisle.lastVault'

export default function App() {
  const [vault, setVault] = useState<ScanResult | null>(null)
  const [activeNote, setActiveNote] = useState<NoteContent | null>(null)
  const [saveState, setSaveState] = useState<SaveState | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [runtime, setRuntime] = useState<string>('')
  const editorRef = useRef<NoteEditorHandle>(null)

  // 启动时 ping 一次：确认 Rust 侧真的起来了。
  // 如果这一句失败，后面所有按钮都会失败，报错信息会误导排查方向。
  useEffect(() => {
    appPing()
      .then((info) => setRuntime(`v${info.version} · pid ${info.pid}`))
      .catch((err) => setRuntime(`IPC 未就绪：${describeError(err)}`))
  }, [])

  // 恢复上次打开的 Vault。失败就静默清掉记录——目录可能被删了、改名了、
  // 或者挪到了没权限的地方，这些都不该在启动时糊用户一脸错误。
  useEffect(() => {
    const last = localStorage.getItem(LAST_VAULT_KEY)
    if (last === null) return
    vaultScan(last)
      .then(setVault)
      .catch(() => localStorage.removeItem(LAST_VAULT_KEY))
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
      setSaveState(null)
      localStorage.setItem(LAST_VAULT_KEY, result.root)
    } catch (err) {
      setVault(null)
      setActiveNote(null)
      setError(describeError(err))
    } finally {
      setBusy(false)
    }
  }, [])

  /**
   * 切换笔记。
   *
   * # 顺序不能换
   *
   * 编辑器的 IPC 合并窗口是 300ms，里面可能还有没发出去的字符。
   * 所以必须「先取内容 → 发给 Rust → flush → 才切换」。
   * 反过来做（先切再 flush）会把最后那几个字丢掉，而且丢得毫无痕迹。
   *
   * # flush 失败就不切
   *
   * 典型失败是冲突（文件被别的程序改过）。此时切走等于把这篇的修改丢了，
   * 所以留在原地，让用户处理冲突横幅。
   */
  const selectNote = useCallback(
    async (node: FileNode) => {
      if (!vault) return
      if (activeNote?.path === node.path) return

      if (activeNote) {
        const content = editorRef.current?.getContent()
        try {
          if (content !== undefined) {
            await noteEdit(vault.root, activeNote.path, content, activeNote.mtime)
          }
          await noteFlush(vault.root, activeNote.path, false)
        } catch (err) {
          setError(describeError(err))
          setSaveState(await noteStatus(vault.root, activeNote.path).catch(() => null))
          return
        }
      }

      try {
        const note = await noteRead(vault.root, node.path)
        setActiveNote(note)
        setSaveState(null)
        setError(null)
      } catch (err) {
        setError(describeError(err))
      }
    },
    [vault, activeNote],
  )

  // 有内容没落盘时才轮询。依赖 `dirty` 而不是整个 `saveState`，
  // 否则每次状态更新都会重建 interval，等于没有轮询。
  const dirty = saveState?.dirty === true
  useEffect(() => {
    if (!vault || !activeNote || !dirty) return
    const timer = window.setInterval(() => {
      noteStatus(vault.root, activeNote.path)
        .then(setSaveState)
        .catch(() => {
          // 轮询失败不打断编辑，下一次还会试。
        })
    }, STATUS_POLL_MS)
    return () => window.clearInterval(timer)
  }, [vault, activeNote, dirty])

  /** 冲突时用户选择「用我的版本覆盖」：跳过乐观锁，直接写。 */
  const forceSave = useCallback(async () => {
    if (!vault || !activeNote) return
    try {
      await noteFlush(vault.root, activeNote.path, true)
      setSaveState(await noteStatus(vault.root, activeNote.path))
      setError(null)
    } catch (err) {
      setError(describeError(err))
    }
  }, [vault, activeNote])

  const conflict = saveState?.lastError?.code === 'CONFLICT'
  const saveLabel = describeSaveState(saveState)

  return (
    <div className="app">
      <header className="topbar">
        <div className="brand">
          <span className="brand__mark">◈</span>
          <span className="brand__name">知屿 KnowIsle</span>
          <span className="brand__tag">M1</span>
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

      {conflict && (
        <div className="banner banner--warn" role="alert">
          <span>
            这篇笔记已被其它程序修改，你的改动还没有保存到磁盘。「覆盖」会丢弃对方的版本。
          </span>
          <button type="button" className="btn btn--small" onClick={forceSave}>
            用我的版本覆盖
          </button>
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
          {vault && activeNote ? (
            <section className="viewer">
              <header className="viewer__header">
                <nav className="breadcrumb" aria-label="路径">
                  {activeNote.path.split('/').map((segment, index) => (
                    <span key={`${segment}-${index}`} className="breadcrumb__item">
                      {index > 0 && <span className="breadcrumb__sep">/</span>}
                      {segment}
                    </span>
                  ))}
                </nav>
                <span className="viewer__meta">{saveLabel}</span>
              </header>

              <NoteEditor
                ref={editorRef}
                root={vault.root}
                note={activeNote}
                onStateChange={setSaveState}
                onError={setError}
              />
            </section>
          ) : (
            <div className="welcome">
              <div className="welcome__mark">◈</div>
              <h1>把知识，安放在自己的岛上。</h1>
              <p>M1 只做一件事：证明「改了不会丢」。</p>
              <p className="hint">
                左侧选择一篇笔记，直接开始写。每 1.5 秒自动保存，断电最多丢 1.5 秒。
                {runtime}
              </p>
            </div>
          )}
        </section>
      </main>
    </div>
  )
}

/** 把保存状态翻成一句人话。空串表示还没开始编辑，不显示。 */
function describeSaveState(state: SaveState | null): string {
  if (state === null) return ''
  if (state.lastError !== null) {
    return state.lastError.code === 'CONFLICT' ? '冲突 · 未保存' : '保存失败'
  }
  if (state.dirty) return '保存中…'
  if (state.lastSavedMtime !== null) {
    return `已保存 ${new Date(state.lastSavedMtime).toLocaleTimeString('zh-CN')}`
  }
  return ''
}
