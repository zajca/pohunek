import type {
  Methods,
  NotificationRecord,
  NotificationUpdateParams,
} from "@pohunek/protocol";

type ActionMethod =
  | "host.inspect"
  | "notification.update"
  | "project.list"
  | "session.detach"
  | "session.inspect"
  | "session.new"
  | "session.resize"
  | "session.stop";

export interface ActionCaller {
  call<K extends ActionMethod>(
    host: string,
    method: K,
    params: Methods[K]["params"],
  ): Promise<Methods[K]["output"]>;
}

export interface NotificationRollback {
  readonly host: string;
  readonly id: string;
  readonly version: number;
  readonly previous: NotificationRecord;
}

export interface OptimisticNotificationCallbacks {
  begin(host: string, params: NotificationUpdateParams): NotificationRollback | undefined;
  commit(host: string, record: NotificationRecord): void;
  rollback(change: NotificationRollback | undefined): void;
}

export class WorkspaceActions {
  private readonly caller: ActionCaller;
  private readonly notifications: OptimisticNotificationCallbacks;

  public constructor(caller: ActionCaller, notifications: OptimisticNotificationCallbacks) {
    this.caller = caller;
    this.notifications = notifications;
  }

  public sessionNew(
    host: string,
    params: Methods["session.new"]["params"],
  ): Promise<Methods["session.new"]["output"]> {
    return this.caller.call(host, "session.new", params);
  }

  public sessionInspect(
    host: string,
    params: Methods["session.inspect"]["params"],
  ): Promise<Methods["session.inspect"]["output"]> {
    return this.caller.call(host, "session.inspect", params);
  }

  public sessionStop(
    host: string,
    params: Methods["session.stop"]["params"],
  ): Promise<Methods["session.stop"]["output"]> {
    return this.caller.call(host, "session.stop", params);
  }

  public sessionResize(
    host: string,
    params: Methods["session.resize"]["params"],
  ): Promise<Methods["session.resize"]["output"]> {
    return this.caller.call(host, "session.resize", params);
  }

  public sessionDetach(
    host: string,
    params: Methods["session.detach"]["params"],
  ): Promise<Methods["session.detach"]["output"]> {
    return this.caller.call(host, "session.detach", params);
  }

  public async notificationUpdate(
    host: string,
    params: Methods["notification.update"]["params"],
  ): Promise<Methods["notification.update"]["output"]> {
    const rollback = this.notifications.begin(host, params);
    try {
      const result = await this.caller.call(host, "notification.update", params);
      this.notifications.commit(host, result.record);
      return result;
    } catch (error: unknown) {
      this.notifications.rollback(rollback);
      throw error;
    }
  }

  public hostInspect(
    host: string,
  ): Promise<Methods["host.inspect"]["output"]> {
    return this.caller.call(host, "host.inspect", null);
  }

  public projectList(
    host: string,
    params: Methods["project.list"]["params"],
  ): Promise<Methods["project.list"]["output"]> {
    return this.caller.call(host, "project.list", params);
  }
}
