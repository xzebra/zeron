import UIKit

extension UIViewController {
    func editProject(_ id: String, app: AppModel, name: String? = nil, iconPath: String? = nil, error: String? = nil) {
        guard let project = app.projectOptions.first(where: { $0.id == id }) else { return }
        let message = error ?? "Icon path is relative to the repository root, for example apps/web/public/icon.png. Leave it blank for automatic."
        let alert = UIAlertController(title: "Edit Project", message: message, preferredStyle: .alert)
        alert.addTextField {
            $0.placeholder = "Project name"
            $0.accessibilityLabel = "Project name"
            $0.text = name ?? project.groupName
        }
        alert.addTextField {
            $0.placeholder = "Icon path (automatic)"
            $0.accessibilityLabel = "Icon path"
            $0.text = iconPath ?? project.iconPath
            $0.autocorrectionType = .no
            $0.autocapitalizationType = .none
            $0.spellCheckingType = .no
        }
        alert.addAction(UIAlertAction(title: "Cancel", style: .cancel))
        alert.addAction(UIAlertAction(title: "Save", style: .default) { [weak self, weak alert] _ in
            guard let self, let alert else { return }
            let name = alert.textFields?[0].text ?? ""
            let path = alert.textFields?[1].text ?? ""
            do {
                // An untouched name keeps the stored one (it may be the folder name).
                let trimmed = name.trimmingCharacters(in: .whitespaces)
                try app.editProject(id, name: trimmed == project.groupName ? nil : trimmed, iconPath: path, colorIndex: project.customColorIndex)
            } catch {
                // Preserve the draft when validation fails or the project disappeared.
                DispatchQueue.main.async {
                    self.editProject(id, app: app, name: name, iconPath: path, error: error.localizedDescription)
                }
            }
        })
        present(alert, animated: true)
    }

    /// The project's monogram color: Automatic (derived from its path) or a
    /// palette slot shared with the desktop.
    func projectColorMenu(_ id: String, app: AppModel) -> UIMenu? {
        guard let project = app.projectOptions.first(where: { $0.id == id }) else { return nil }
        let names = ["Slate", "Blue", "Violet", "Rose", "Amber", "Emerald", "Teal", "Orange"]
        let choices: [(String, Int?)] = [("Automatic", nil)] + names.enumerated().map { ($0.element, $0.offset) }
        return UIMenu(title: "Project Color", image: UIImage(systemName: "paintpalette"), children: choices.map { title, index in
            let tile = index.map { ProjectTile.image(name: project.groupName, colorIndex: $0, side: 18) }
            return UIAction(title: title, image: tile, state: index == project.customColorIndex ? .on : .off) { [weak self] _ in
                do {
                    try app.editProject(id, name: nil, iconPath: project.iconPath, colorIndex: index)
                } catch {
                    let alert = UIAlertController(title: "Couldn’t Change Color", message: error.localizedDescription, preferredStyle: .alert)
                    alert.addAction(UIAlertAction(title: "OK", style: .default))
                    self?.present(alert, animated: true)
                }
            }
        })
    }
}
