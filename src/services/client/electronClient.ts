import type {
  IPlayerClient,
  LoadOptions,
  LoadResult,
  IpcResponse,
  PlayerStatus,
  AudioDevice,
  FftData,
  PlayerEvent,
} from "./types";

/**
 * Electron IPC 播放器客户端实现
 * 直接调用 window.api.player 提供的 IPC 接口
 */
export class ElectronPlayerClient implements IPlayerClient {
  private get api() {
    if (typeof window === "undefined" || !window.api?.player) {
      throw new Error("Electron API is not available in current environment");
    }
    return window.api.player;
  }

  load(source: string, options?: LoadOptions): Promise<IpcResponse<LoadResult>> {
    return this.api.load(source, options);
  }

  play(): Promise<IpcResponse> {
    return this.api.play();
  }

  pause(): Promise<IpcResponse> {
    return this.api.pause();
  }

  stop(): Promise<IpcResponse> {
    return this.api.stop();
  }

  seek(positionMs: number): Promise<IpcResponse> {
    return this.api.seek(positionMs);
  }

  setVolume(volume: number): Promise<IpcResponse> {
    return this.api.setVolume(volume);
  }

  setPauseOnDeviceSwitch(enabled: boolean): Promise<IpcResponse> {
    return this.api.setPauseOnDeviceSwitch(enabled);
  }

  getVolume(): Promise<IpcResponse<number>> {
    return this.api.getVolume();
  }

  getStatus(): Promise<IpcResponse<PlayerStatus>> {
    return this.api.getStatus();
  }

  stageDirectNext(
    source: string,
    durationSecs: number,
    generation?: number,
  ): Promise<IpcResponse<boolean>> {
    return this.api.stageDirectNext(source, durationSecs, generation);
  }

  cancelDirectNext(): Promise<IpcResponse> {
    return this.api.cancelDirectNext();
  }

  commitDirectBoundary(source: string, durationSecs: number): Promise<IpcResponse> {
    return this.api.commitDirectBoundary(source, durationSecs);
  }

  setFftEnabled(enabled: boolean): Promise<IpcResponse> {
    return this.api.setFftEnabled(enabled);
  }

  getFftData(): Promise<IpcResponse<FftData>> {
    return this.api.getFftData();
  }

  setFadeDuration(ms: number): Promise<IpcResponse> {
    return this.api.setFadeDuration(ms);
  }

  getFadeDuration(): Promise<IpcResponse<number>> {
    return this.api.getFadeDuration();
  }

  getCoverRaw(): Promise<IpcResponse<string | null>> {
    return this.api.getCoverRaw();
  }

  readLyricFile(filePath: string): Promise<IpcResponse<string>> {
    return this.api.readLyricFile(filePath);
  }

  reinit(): Promise<IpcResponse> {
    return this.api.reinit();
  }

  setNormalizationEnabled(enabled: boolean): Promise<IpcResponse> {
    return this.api.setNormalizationEnabled(enabled);
  }

  setEqualizerEnabled(enabled: boolean): Promise<IpcResponse> {
    return this.api.setEqualizerEnabled(enabled);
  }

  setEqualizerBands(gainsDb: number[]): Promise<IpcResponse> {
    return this.api.setEqualizerBands(gainsDb);
  }

  setPreampGain(preampDb: number): Promise<IpcResponse> {
    return this.api.setPreampGain(preampDb);
  }

  setSpeed(speed: number): Promise<IpcResponse> {
    return this.api.setSpeed(speed);
  }

  setPitch(semitones: number): Promise<IpcResponse> {
    return this.api.setPitch(semitones);
  }

  setPitchSync(sync: boolean): Promise<IpcResponse> {
    return this.api.setPitchSync(sync);
  }

  getOutputDevices(): Promise<IpcResponse<AudioDevice[]>> {
    return this.api.getOutputDevices();
  }

  getDefaultDeviceName(): Promise<IpcResponse<string | null>> {
    return this.api.getDefaultDeviceName();
  }

  setOutputDevice(deviceName: string | null): Promise<IpcResponse> {
    return this.api.setOutputDevice(deviceName);
  }

  getSelectedDeviceName(): Promise<IpcResponse<string | null>> {
    return this.api.getSelectedDeviceName();
  }

  syncPlayMode(repeatMode: string, shuffleMode: string): void {
    this.api.syncPlayMode(repeatMode, shuffleMode);
  }

  syncLikeState(liked: boolean): void {
    this.api.syncLikeState(liked);
  }

  dispatch(type: string): void {
    this.api.dispatch(type);
  }

  onEvent(callback: (event: PlayerEvent) => void): () => void {
    return this.api.onEvent(callback);
  }

  async scanDirettaTargets(): Promise<IpcResponse<any[]>> {
    return { success: true, data: [] };
  }

  async getDirettaStatus(): Promise<IpcResponse<any>> {
    return { success: true, data: { selected_device: null, is_diretta_active: false } };
  }

  async selectDirettaTarget(target: string | null): Promise<IpcResponse<any>> {
    return this.setOutputDevice(target);
  }

  async getDirettaTargetInfo(target: string): Promise<IpcResponse<any>> {
    // 桌面模式未实现 Diretta Target 能力查询（scanDirettaTargets 恒为空列表），
    // 返回失败以触发前端空态，避免展示伪造数据
    void target;
    return {
      success: false,
      error: "Diretta target capability query is only available in headless server mode",
    };
  }

  async browseFs(_path?: string): Promise<IpcResponse<any>> {
    return { success: false, error: "browseFs is only available in headless server mode" };
  }
}
