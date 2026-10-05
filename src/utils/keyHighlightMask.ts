/** 内置贴图的爪子是灰白色，键位是青色。只提取青色，绝不复制爪子。 */
export function extractKeyHighlight(data: Uint8ClampedArray, width: number, height: number) {
  let left = width
  let top = height
  let right = -1
  let bottom = -1
  const isHighlight = (i: number) => data[i + 3] > 0
    && data[i + 1] - data[i] > 8 && data[i + 2] - data[i] > 8

  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      if (!isHighlight((y * width + x) * 4)) continue
      left = Math.min(left, x)
      top = Math.min(top, y)
      right = Math.max(right, x)
      bottom = Math.max(bottom, y)
    }
  }
  if (right < left) return null

  const cropWidth = right - left + 1
  const cropHeight = bottom - top + 1
  const pixels = new Uint8ClampedArray(cropWidth * cropHeight * 4)
  for (let y = top; y <= bottom; y++) {
    for (let x = left; x <= right; x++) {
      const source = (y * width + x) * 4
      if (isHighlight(source)) {
        pixels.set(data.subarray(source, source + 4), ((y - top) * cropWidth + x - left) * 4)
      }
    }
  }
  return { left, top, width: cropWidth, height: cropHeight, pixels }
}
