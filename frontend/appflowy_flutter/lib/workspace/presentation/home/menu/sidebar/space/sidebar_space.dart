import 'package:appflowy/features/shared_section/presentation/shared_section.dart';
import 'package:appflowy/features/workspace/logic/workspace_bloc.dart';
import 'package:appflowy/shared/feature_flags.dart';
import 'package:appflowy/startup/startup.dart';
import 'package:appflowy/workspace/application/favorite/favorite_bloc.dart';
import 'package:appflowy/workspace/application/sidebar/space/space_bloc.dart';
import 'package:appflowy/workspace/application/tabs/tabs_bloc.dart';
import 'package:appflowy/workspace/presentation/home/hotkeys.dart';
import 'package:appflowy/workspace/presentation/home/menu/menu_shared_state.dart';
import 'package:appflowy/workspace/presentation/home/menu/sidebar/favorites/favorite_folder.dart';
import 'package:appflowy/workspace/presentation/home/menu/sidebar/space/create_space_popup.dart';
import 'package:appflowy/workspace/presentation/home/menu/sidebar/space/shared_widget.dart';
import 'package:appflowy/workspace/presentation/home/menu/sidebar/space/sidebar_space_header.dart';
import 'package:appflowy_backend/protobuf/flowy-folder/view.pb.dart'
    hide AFRolePB;
import 'package:appflowy_backend/protobuf/flowy-user/protobuf.dart';
import 'package:appflowy_editor/appflowy_editor.dart';
import 'package:flowy_infra_ui/flowy_infra_ui.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_bloc/flutter_bloc.dart';
import 'package:provider/provider.dart';

class SidebarSpace extends StatelessWidget {
  const SidebarSpace({
    super.key,
    this.isHoverEnabled = true,
    required this.userProfile,
  });

  final bool isHoverEnabled;
  final UserProfilePB userProfile;

  @override
  Widget build(BuildContext context) {
    final currentWorkspace =
        context.watch<UserWorkspaceBloc>().state.currentWorkspace;
    final currentWorkspaceId = currentWorkspace?.workspaceId ?? '';

    // only show spaces if the user role is member or owner
    final currentUserRole = currentWorkspace?.role;
    final shouldShowSpaces = [
      AFRolePB.Member,
      AFRolePB.Owner,
    ].contains(currentUserRole);

    return ValueListenableBuilder(
      valueListenable: getIt<MenuSharedState>().notifier,
      builder: (_, __, ___) => Provider.value(
        value: userProfile,
        child: Column(
          children: [
            const VSpace(4.0),
            // favorite
            BlocBuilder<FavoriteBloc, FavoriteState>(
              builder: (context, state) {
                if (state.views.isEmpty) {
                  return const SizedBox.shrink();
                }
                return FavoriteFolder(
                  views: state.views.map((e) => e.item).toList(),
                );
              },
            ),

            // shared
            if (FeatureFlag.sharedSection.isOn) ...[
              const VSpace(16.0),
              SharedSection(
                key: ValueKey(currentWorkspaceId),
                workspaceId: currentWorkspaceId,
              ),
            ],

            // spaces
            if (shouldShowSpaces) ...[
              const VSpace(16.0),
              // spaces
              const _Space(),
            ],

            const VSpace(200),
          ],
        ),
      ),
    );
  }
}

class _Space extends StatefulWidget {
  const _Space();

  @override
  State<_Space> createState() => _SpaceState();
}

class _SpaceState extends State<_Space> {
  final isHovered = ValueNotifier(false);
  final isExpandedNotifier = PropertyValueNotifier(false);

  @override
  void initState() {
    super.initState();

    switchToTheNextSpace.addListener(_switchToNextSpace);
    switchToSpaceNotifier.addListener(_switchToSpace);
  }

  @override
  void dispose() {
    switchToTheNextSpace.removeListener(_switchToNextSpace);
    isHovered.dispose();
    isExpandedNotifier.dispose();

    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final currentWorkspace =
        context.watch<UserWorkspaceBloc>().state.currentWorkspace;
    return BlocBuilder<SpaceBloc, SpaceState>(
      builder: (context, state) {
        if (state.spaces.isEmpty) {
          return const SizedBox.shrink();
        }

        final currentSpace = state.currentSpace ?? state.spaces.first;

        return Column(
          children: [
            SidebarSpaceHeader(
              isExpanded: state.isExpanded,
              space: currentSpace,
              onAdded: (layout) => _showCreatePagePopup(
                context,
                currentSpace,
                layout,
              ),
              onCreateNewSpace: () => _showCreateSpaceDialog(context),
              onCollapseAllPages: () => isExpandedNotifier.value = true,
            ),
            if (state.isExpanded)
              MouseRegion(
                onEnter: (_) => isHovered.value = true,
                onExit: (_) => isHovered.value = false,
                child: SpacePages(
                  key: ValueKey(
                    Object.hashAll([
                      currentWorkspace?.workspaceId ?? '',
                      currentSpace.id,
                    ]),
                  ),
                  isExpandedNotifier: isExpandedNotifier,
                  space: currentSpace,
                  isHovered: isHovered,
                  onSelected: (context, view) {
                    if (HardwareKeyboard.instance.isControlPressed) {
                      context.read<TabsBloc>().openTab(view);
                    }
                    context.read<TabsBloc>().openPlugin(view);
                  },
                  onTertiarySelected: (context, view) =>
                      context.read<TabsBloc>().openTab(view),
                ),
              ),
          ],
        );
      },
    );
  }

  void _showCreateSpaceDialog(BuildContext context) {
    final spaceBloc = context.read<SpaceBloc>();
    showDialog(
      context: context,
      builder: (_) => Dialog(
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(12.0),
        ),
        child: BlocProvider.value(
          value: spaceBloc,
          child: const CreateSpacePopup(),
        ),
      ),
    );
  }

  void _showCreatePagePopup(
    BuildContext context,
    ViewPB space,
    ViewLayoutPB layout,
  ) {
    context.read<SpaceBloc>().add(
          SpaceEvent.createPage(
            name: '',
            layout: layout,
            index: 0,
            openAfterCreate: true,
          ),
        );

    context.read<SpaceBloc>().add(SpaceEvent.expand(space, true));
  }

  void _switchToNextSpace() {
    context.read<SpaceBloc>().add(const SpaceEvent.switchToNextSpace());
  }

  void _switchToSpace() {
    if (!mounted || !context.mounted) {
      return;
    }

    final space = switchToSpaceNotifier.value;
    if (space == null) {
      return;
    }

    context.read<SpaceBloc>().add(SpaceEvent.open(space: space));
  }
}
