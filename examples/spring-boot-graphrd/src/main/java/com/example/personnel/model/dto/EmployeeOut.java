package com.example.personnel.model.dto;

import com.example.personnel.model.dto.EmployeeOut;
import com.example.personnel.model.entity.Employee;

/** How an employee appears outside: the company joined in by its IRI. */
public record EmployeeOut(String id, String name, String email, Integer age,
                          Integer salary, String hired, String companyName) {

    /** The outbound shape of an entity. */
    public static EmployeeOut of(Employee e) {
        return new EmployeeOut(e.getId(), e.getName(), e.getEmail(), e.getAge(),
                e.getSalary(), e.getHired(), e.getWorksFor() == null ? null : e.getWorksFor().getName());
    }
}
