import type {
  Methods,
  NotificationRecord,
  NotificationUpdateParams,
} from "@pohunek/protocol";

type ActionMethod =
  | "host.inspect"
  | "notification.policy.get"
  | "notification.policy.set"
  | "notification.update"
  | "project.add"
  | "project.list"
  | "project.remove"
  | "project.rename"
  | "project.show"
  | "session.detach"
  | "session.fork"
  | "session.inspect"
  | "session.new"
  | "session.remove"
  | "session.screen"
  | "session.rename"
  | "session.resume"
  | "session.resize"
  | "session.set_metadata"
  | "session.stop"
  | "worktree.remove";

export interface ActionCaller {
  call<K extends ActionMethod>(
    host: string,
    method: K,
    params: Methods[K]["params"],
  ): Promise<Methods[K]["output"]>;
  sessionOutput(
    host: string,
    params: Methods["session.output"]["params"],
  ): Promise<Methods["session.output"]["output"]>;
  sessionWait(
    host: string,
    params: Methods["session.wait"]["params"],
  ): Promise<Methods["session.wait"]["output"]>;
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

  public sessionScreen(
    host: string,
    params: Methods["session.screen"]["params"],
  ): Promise<Methods["session.screen"]["output"]> {
    return this.caller.call(host, "session.screen", params);
  }

  public sessionOutput(
    host: string,
    params: Methods["session.output"]["params"],
  ): Promise<Methods["session.output"]["output"]> {
    return this.caller.sessionOutput(host, params);
  }

  public sessionWait(
    host: string,
    params: Methods["session.wait"]["params"],
  ): Promise<Methods["session.wait"]["output"]> {
    return this.caller.sessionWait(host, params);
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

  public notificationPolicyGet(
    host: string,
  ): Promise<Methods["notification.policy.get"]["output"]> {
    return this.caller.call(host, "notification.policy.get", null);
  }

  public notificationPolicySet(
    host: string,
    params: Methods["notification.policy.set"]["params"],
  ): Promise<Methods["notification.policy.set"]["output"]> {
    return this.caller.call(host, "notification.policy.set", params);
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

  public sessionRename(
    host: string,
    params: Methods["session.rename"]["params"],
  ): Promise<Methods["session.rename"]["output"]> {
    return this.caller.call(host, "session.rename", params);
  }

  public sessionSetMetadata(
    host: string,
    params: Methods["session.set_metadata"]["params"],
  ): Promise<Methods["session.set_metadata"]["output"]> {
    return this.caller.call(host, "session.set_metadata", params);
  }

  public sessionResume(
    host: string,
    params: Methods["session.resume"]["params"],
  ): Promise<Methods["session.resume"]["output"]> {
    return this.caller.call(host, "session.resume", params);
  }

  public sessionFork(
    host: string,
    params: Methods["session.fork"]["params"],
  ): Promise<Methods["session.fork"]["output"]> {
    return this.caller.call(host, "session.fork", params);
  }

  public sessionRemove(
    host: string,
    params: Methods["session.remove"]["params"],
  ): Promise<Methods["session.remove"]["output"]> {
    return this.caller.call(host, "session.remove", params);
  }

  public projectAdd(
    host: string,
    params: Methods["project.add"]["params"],
  ): Promise<Methods["project.add"]["output"]> {
    return this.caller.call(host, "project.add", params);
  }

  public projectShow(
    host: string,
    params: Methods["project.show"]["params"],
  ): Promise<Methods["project.show"]["output"]> {
    return this.caller.call(host, "project.show", params);
  }

  public projectRename(
    host: string,
    params: Methods["project.rename"]["params"],
  ): Promise<Methods["project.rename"]["output"]> {
    return this.caller.call(host, "project.rename", params);
  }

  public projectRemove(
    host: string,
    params: Methods["project.remove"]["params"],
  ): Promise<Methods["project.remove"]["output"]> {
    return this.caller.call(host, "project.remove", params);
  }

  public worktreeRemove(
    host: string,
    params: Methods["worktree.remove"]["params"],
  ): Promise<Methods["worktree.remove"]["output"]> {
    return this.caller.call(host, "worktree.remove", params);
  }
}
