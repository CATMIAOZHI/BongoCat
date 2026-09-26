function version(value: string): number[] | undefined {
  const match = /^v?(\d+)\.(\d+)\.(\d+)$/.exec(value)
  return match?.slice(1).map(Number)
}

function compare(a: number[], b: number[]): number {
  return a[0]! - b[0]! || a[1]! - b[1]! || a[2]! - b[2]!
}

/** 同仓库还发布 relay-v*，不能把服务器版本当客户端更新。 */
export function latestClientTag(releases: unknown, current: string): string | undefined {
  const installed = version(current)
  if (!installed || !Array.isArray(releases)) throw new Error('Invalid release data')
  return releases
    .filter((item): item is { tag_name: string } => !!item
      && typeof item.tag_name === 'string'
      && !item.draft && !item.prerelease
      && /^v\d+\.\d+\.\d+$/.test(item.tag_name))
    .filter(item => compare(version(item.tag_name)!, installed) > 0)
    .sort((a, b) => compare(version(b.tag_name)!, version(a.tag_name)!))[0]
    ?.tag_name
}
