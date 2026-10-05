import type { MotionInfo } from 'easy-live2d'

import { convertFileSrc } from '@tauri-apps/api/core'
import { readDir, readTextFile } from '@tauri-apps/plugin-fs'
import { Config, CubismSetting, Live2DSprite, Priority } from 'easy-live2d'
import { groupBy } from 'es-toolkit/compat'
import JSON5 from 'json5'
import { Application, Ticker } from 'pixi.js'

import type { ModelSize } from '@/composables/useModel'

import { i18n } from '@/locales'

import { join } from './path'

Config.MouseFollow = false

class Live2d {
  private app: Application | null = null
  public model: Live2DSprite | null = null
  /** 设置里那个「最大帧率」的值。可能比 `app` 先到（`main` 页的 watch 是 immediate） */
  private maxFPS: number | null = null

  constructor() { }

  private initApp() {
    if (this.app) return

    const view = document.getElementById('live2dCanvas') as HTMLCanvasElement | null

    this.app = new Application()

    const app = this.app

    /*
     * 画布跟着**它所在的那块区域**走，而不是整个窗口。
     *
     * 猫咪窗口开着双人联机时，顶上有一条聊天浮层（R39），猫只占下面那一块；按窗口大小
     * 摆猫会把猫整体往下推一个浮层的高度、底部被裁掉。对方猫咪窗口与单机时这块区域就是
     * 整个窗口，行为不变。
     */
    return app.init({
      view: view ?? void 0,
      resizeTo: view?.parentElement ?? window,
      backgroundAlpha: 0,
      autoStart: false,
      autoDensity: true,
      resolution: devicePixelRatio,
    }).then(() => {
      // `ticker` 要等 `init` 之后才在，所以这里补一次（见 `setMaxFPS`）
      if (this.maxFPS !== null) app.ticker.maxFPS = this.maxFPS
    })
  }

  public async load(path: string) {
    await this.initApp()

    this.destroy()

    const files = await readDir(path)

    const modelFile = files.find(file => file.name.endsWith('.model3.json'))

    if (!modelFile) {
      throw new Error(i18n.global.t('utils.live2d.hints.notFound'))
    }

    const modelPath = join(path, modelFile.name)

    const modelJSON = JSON5.parse(await readTextFile(modelPath))

    const modelSetting = new CubismSetting({
      modelJSON,
    })

    modelSetting.redirectPath(({ file }) => {
      return convertFileSrc(join(path, file))
    })

    this.model = new Live2DSprite({
      modelSetting,
      ticker: Ticker.shared,
    })

    this.app?.stage.addChild(this.model)
    // easy-live2d 在首次 renderFrame 中启动加载，必须在等待 ready 前恢复渲染。
    this.app?.start()

    await this.model.ready

    const { width, height } = this.model

    const motions = groupBy(this.model.getMotions(), 'group')
    const expressions = this.model.getExpressions()

    return {
      width,
      height,
      motions,
      expressions,
    }
  }

  public destroy() {
    this.app?.ticker?.stop()

    if (!this.model) return

    this.model?.destroy()

    this.model = null
    // 清掉最后一帧，但不为一张空画布持续请求动画帧。
    this.app?.render()
  }

  public resizeModel(modelSize: ModelSize) {
    if (!this.model) return

    const { width, height } = modelSize
    const area = this.app?.canvas.parentElement
    const areaWidth = area?.clientWidth || innerWidth
    const areaHeight = area?.clientHeight || innerHeight

    const scaleX = areaWidth / width
    const scaleY = areaHeight / height
    const scale = Math.min(scaleX, scaleY)

    this.model.scale.set(scale)
    this.model.x = areaWidth / 2
    this.model.y = areaHeight / 2
    this.model.anchor.set(0.5)
  }

  public startMotion(motion: MotionInfo) {
    return this.model?.startMotion({
      ...motion,
      priority: Priority.Normal,
    })
  }

  public setExpression(index: number) {
    return this.model?.setExpression({ index })
  }

  public getParameterValueRange(id: string) {
    return this.model?.getParameterValueRangeById(id)
  }

  public setParameterValue(id: string, value: number | boolean) {
    return this.model?.setParameterValueById(id, Number(value))
  }

  public setMotionSoundEnabled(enabled: boolean) {
    Config.MotionSound = enabled
  }

  public setMaxFPS(fps: number) {
    this.maxFPS = fps

    /*
     * 渲染循环是 pixi `Application` **自己的** ticker（`sharedTicker` 默认 false、`autoStart`
     * 默认 true），所以只设 `Ticker.shared.maxFPS` 等于这个设置项完全不生效：猫会一直按显示器
     * 刷新率渲染（144/165Hz 的屏就是 144/165fps），白占一格合成通道，和全屏游戏 + 推流抢 GPU。
     *
     * `Ticker.shared` 那份也要设：本机的鼠标插值挂在它上面（`useDevice`）。
     */
    Ticker.shared.maxFPS = fps

    /*
     * `ticker` 是 pixi 的 `TickerPlugin` 在 `Application.init()` **内部**才挂上去的，而 `this.app`
     * 在 `initApp` 开头就赋值了——这两者之间正好可能打进来一次跨窗口的设置同步（用户刚打开
     * 设置页改帧率）。所以要判的是 `ticker` 而不是 `app`：那个窗口期里补设由 `init()` 的
     * `.then()` 负责（`this.maxFPS` 上面已经存下了）。
     */
    const ticker = this.app?.ticker

    if (ticker) ticker.maxFPS = fps
  }
}

const live2d = new Live2d()

export default live2d
