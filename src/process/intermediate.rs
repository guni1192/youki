use crate::{namespaces::Namespaces, process::channel, process::fork};
use anyhow::{Context, Error, Result};
use cgroups::common::CgroupManager;
use nix::unistd::{Gid, Pid, Uid};
use oci_spec::runtime::{LinuxNamespaceType, LinuxResources};
use procfs::process::Process;
use std::convert::From;

use super::args::ContainerArgs;
use super::init::container_init;

pub fn container_intermediate(
    args: ContainerArgs,
    receiver_from_main: &mut channel::ReceiverFromMain,
    sender_to_main: &mut channel::SenderIntermediateToMain,
) -> Result<()> {
    let command = &args.syscall;
    let spec = &args.spec;
    let linux = spec.linux().as_ref().context("no linux in spec")?;
    let namespaces = Namespaces::from(linux.namespaces().as_ref());

    // if new user is specified in specification, this will be true and new
    // namespace will be created, check
    // https://man7.org/linux/man-pages/man7/user_namespaces.7.html for more
    // information
    if let Some(user_namespace) = namespaces.get(LinuxNamespaceType::User) {
        namespaces
            .unshare_or_setns(user_namespace)
            .with_context(|| format!("Failed to enter user namespace: {:?}", user_namespace))?;
        if user_namespace.path().is_none() {
            log::debug!("creating new user namespace");
            // child needs to be dumpable, otherwise the non root parent is not
            // allowed to write the uid/gid maps
            prctl::set_dumpable(true).unwrap();
            sender_to_main.identifier_mapping_request()?;
            receiver_from_main.wait_for_mapping_ack()?;
            prctl::set_dumpable(false).unwrap();
        }

        // After UID and GID mapping is configured correctly in the Youki main
        // process, We want to make sure continue as the root user inside the
        // new user namespace. This is required because the process of
        // configuring the container process will require root, even though the
        // root in the user namespace likely is mapped to an non-priviliged user
        // on the parent user namespace.
        command.set_id(Uid::from_raw(0), Gid::from_raw(0)).context(
            "Failed to configure uid and gid root in the beginning of a new user namespace",
        )?;
    }

    // set limits and namespaces to the process
    let proc = spec.process().as_ref().context("no process in spec")?;
    if let Some(rlimits) = proc.rlimits() {
        for rlimit in rlimits {
            command.set_rlimit(rlimit).context("failed to set rlimit")?;
        }
    }

    // Pid namespace requires an extra fork to enter, so we enter pid namespace now.
    if let Some(pid_namespace) = namespaces.get(LinuxNamespaceType::Pid) {
        namespaces
            .unshare_or_setns(pid_namespace)
            .with_context(|| format!("Failed to enter pid namespace: {:?}", pid_namespace))?;
    }

    // this needs to be done before we create the init process, so that the init
    // process will already be captured by the cgroup
    if args.rootless.is_none() {
        apply_cgroups(
            args.cgroup_manager.as_ref(),
            linux.resources().as_ref(),
            args.init,
        )
        .context("failed to apply cgroups")?
    }

    // We only need for init process to send us the ChildReady.
    let (sender_to_intermediate, receiver_from_init) = &mut channel::init_to_intermediate()?;

    // We have to record the pid of the child (container init process), since
    // the child will be inside the pid namespace. We can't rely on child_ready
    // to send us the correct pid.
    let pid = fork::container_fork(|| {
        // First thing in the child process to close the unused fds in the channel/pipe.
        receiver_from_init
            .close()
            .context("Failed to close receiver in init process")?;
        container_init(args, sender_to_intermediate)
    })?;
    // Close unused fds in the parent process.
    sender_to_intermediate
        .close()
        .context("Failed to close sender in the intermediate process")?;
    // There is no point using the pid returned here, since the child will be
    // inside the pid namespace already.
    receiver_from_init
        .wait_for_init_ready()
        .context("Failed to wait for the child")?;
    // After the child (the container init process) becomes ready, we can signal
    // the parent (the main process) that we are ready.
    sender_to_main
        .intermediate_ready(pid)
        .context("Failed to send child ready from intermediate process")?;

    Ok(())
}

fn apply_cgroups<C: CgroupManager + ?Sized>(
    cmanager: &C,
    resources: Option<&LinuxResources>,
    init: bool,
) -> Result<(), Error> {
    let pid = Pid::from_raw(Process::myself()?.pid());
    cmanager
        .add_task(pid)
        .with_context(|| format!("failed to add task {} to cgroup manager", pid))?;

    if resources.is_some() && init {
        let controller_opt = cgroups::common::ControllerOpt {
            resources: resources.unwrap(),
            freezer_state: None,
            oom_score_adj: None,
            disable_oom_killer: false,
        };

        cmanager
            .apply(&controller_opt)
            .context("failed to apply resource limits to cgroup")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::apply_cgroups;
    use anyhow::Result;
    use cgroups::test_manager::TestManager;
    use nix::unistd::Pid;
    use oci_spec::runtime::LinuxResources;
    use procfs::process::Process;

    #[test]
    fn apply_cgroup_init() -> Result<()> {
        // arrange
        let cmanager = TestManager::default();
        let resources = LinuxResources::default();

        // act
        apply_cgroups(&cmanager, Some(&resources), true)?;

        // assert
        assert!(cmanager.get_add_task_args().len() == 1);
        assert_eq!(
            cmanager.get_add_task_args()[0],
            Pid::from_raw(Process::myself()?.pid())
        );
        assert_eq!(cmanager.apply_called(), true);
        Ok(())
    }

    #[test]
    fn apply_cgroup_tenant() -> Result<()> {
        // arrange
        let cmanager = TestManager::default();
        let resources = LinuxResources::default();

        // act
        apply_cgroups(&cmanager, Some(&resources), false)?;

        // assert
        assert_eq!(
            cmanager.get_add_task_args()[0],
            Pid::from_raw(Process::myself()?.pid())
        );
        assert_eq!(cmanager.apply_called(), false);
        Ok(())
    }

    #[test]
    fn apply_cgroup_no_resources() -> Result<()> {
        // arrange
        let cmanager = TestManager::default();

        // act
        apply_cgroups(&cmanager, None, true)?;
        // assert
        assert_eq!(
            cmanager.get_add_task_args()[0],
            Pid::from_raw(Process::myself()?.pid())
        );
        assert_eq!(cmanager.apply_called(), false);
        Ok(())
    }
}
