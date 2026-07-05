//! `worktree.*` methods.

use protocol::{Request, Response, WorktreeRemoveParams};

use super::util::{ok_value, parse_params};
use crate::session::SessionRegistry;

/// `worktree.remove`: remove a single pohunek-owned worktree by path. Fail-closed
/// — refuses an external (unowned) worktree (`worktree_not_owned`) and one a live
/// session still uses (`worktree_in_use`); never touches the main checkout.
pub(super) async fn handle_worktree_remove(
    request: &Request,
    sessions: &SessionRegistry,
) -> Response {
    let params = match parse_params::<WorktreeRemoveParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions.remove_worktree(&params.path).await {
        Ok(result) => ok_value(request, &result),
        Err(err) => Response::err(request.id.clone(), err),
    }
}
