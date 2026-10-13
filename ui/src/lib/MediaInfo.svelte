<!-- 媒体信息：每条音视频 / 字幕流一张卡，底部是文件名。 -->
<script lang="ts">
  import { Captions, Check, Video, Volume2 } from '@lucide/svelte'
  import { bitrate } from './fmt'
  import type { MediaSource, MediaStream } from './types'

  let { source }: { source: MediaSource } = $props()
  const yn = (b?: boolean) => (b ? '是' : '否')
  const LABEL: Record<string, string> = { Video: '视频', Audio: '音频', Subtitle: '字幕' }

  function rows(s: MediaStream): [string, unknown][] {
    if (s.Type === 'Video')
      return [
        ['标题', s.DisplayTitle], ['编码', s.Codec], ['分辨率', s.Width && `${s.Width}×${s.Height}`],
        ['帧率', s.RealFrameRate ?? s.AverageFrameRate], ['比特率', bitrate(s.BitRate)], ['动态范围', s.VideoRange],
        ['配置', s.Profile], ['等级', s.Level], ['长宽比', s.AspectRatio], ['交错', yn(s.IsInterlaced)],
        ['位深', s.BitDepth], ['像素格式', s.PixelFormat],
      ]
    if (s.Type === 'Audio')
      return [
        ['标题', s.DisplayTitle], ['语言', s.Language], ['布局', s.ChannelLayout], ['声道', s.Channels], ['编码', s.Codec],
        ['采样率', s.SampleRate && `${s.SampleRate} Hz`], ['位深', s.BitDepth], ['外部', yn(s.IsExternal)], ['默认', yn(s.IsDefault)],
      ]
    return [
      ['标题', s.DisplayTitle], ['内嵌标题', s.Title], ['语言', s.Language], ['编码', s.Codec],
      ['外部', yn(s.IsExternal)], ['默认', yn(s.IsDefault)],
    ]
  }
  const streams = $derived((source.MediaStreams ?? []).filter((s) => s.Type in LABEL))
  const file = $derived(source.Path?.split(/[\\/]/).pop() ?? source.Name ?? '')
</script>

<div class="cards">
  {#each streams as s (s.Index)}
    <div class="card">
      <div class="head">
        {#if s.Type === 'Video'}<Video size={16} />{:else if s.Type === 'Audio'}<Volume2 size={16} />{:else}<Captions size={16} />{/if}
        {LABEL[s.Type]}
        {#if s.IsDefault && s.Type !== 'Video'}<span class="def"><Check size={12} strokeWidth={3} /> 默认</span>{/if}
      </div>
      {#each rows(s).filter(([, v]) => v != null && v !== '') as [k, v] (k)}
        <div class="kv"><span class="k">{k}</span><span class="v selectable">{v}</span></div>
      {/each}
    </div>
  {/each}
</div>
{#if file}<div class="file selectable">{file}</div>{/if}

<style>
  .cards { display: flex; gap: 12px; overflow-x: auto; padding: 0 var(--gutter) 4px; scrollbar-width: none; }
  .card { flex: none; width: 280px; border-radius: var(--radius-l); padding: 14px 16px; background: var(--bg-raised); box-shadow: inset 0 0 0 1px var(--stroke); }
  .head { display: flex; align-items: center; gap: 8px; font-size: 14px; font-weight: 600; margin-bottom: 8px; }
  .def { margin-left: auto; display: inline-flex; align-items: center; gap: 3px; font-size: 11px; font-weight: 500; color: var(--accent); }
  .kv { display: flex; gap: 12px; font-size: 12px; line-height: 1.95; }
  .v { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .k { color: var(--text-2); flex: none; width: 56px; }
  .file { margin: 12px var(--gutter) 0; padding: 0 2px; font-size: 12px; color: var(--text-3); word-break: break-all; }
</style>
