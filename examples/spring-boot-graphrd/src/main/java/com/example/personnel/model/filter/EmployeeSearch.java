package com.example.personnel.model.filter;

/** The free-form employee filters the listing accepts (all optional). */
public record EmployeeSearch(Integer minAge, Integer maxAge, String nameStarts, String company) {

    public boolean isEmpty() {
        return minAge == null && maxAge == null
                && (nameStarts == null || nameStarts.isBlank())
                && (company == null || company.isBlank());
    }
}
